<script setup lang="ts">
import {
  computed,
  getCurrentInstance,
  nextTick,
  onBeforeUnmount,
  onMounted,
  ref,
  watch,
} from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import AppShell from "../components/layout/AppShell.vue";
import Avatar from "../components/Avatar.vue";
import LanguageToggle from "../components/LanguageToggle.vue";
import ThemeToggle from "../components/ThemeToggle.vue";
import { apiErrorMessage } from "../lib/api/messages";
import {
  GROUP_DESCRIPTION_MAX,
  GROUP_TITLE_MAX,
  groupApiErrorMessage,
} from "../lib/api/groups";
import type { GroupInfo, GroupMember, GroupRole } from "../lib/api/groups";
import {
  canChangeRole,
  canInviteMembers,
  canKickMember,
  canLeaveGroup,
  canSetMemberTitle,
  canTransferOwnership,
  roleLabelKey,
} from "../lib/groupRoles";
import { fileToAvatarDataUrl } from "../lib/avatarImage";
import { groupTierTitleKey } from "../lib/groupLevels";
import { APP_VERSION } from "../lib/version";
import { persistLocale } from "../i18n";
import type { AppLocale } from "../i18n";
import { useTheme } from "../composables/useTheme";
import type { ThemePreference } from "../composables/useTheme";
import { apiBase } from "../lib/apiConfig";
import { EMOJIS } from "../lib/emoji";
import {
  MediaUploadError,
  checkMediaFile,
  formatBytes,
  maxBytesForKind,
  uploadMedia,
} from "../lib/api/media";
import type { MediaKind } from "../lib/api/media";
import {
  GROUP_FILE_MAX_BYTES,
  GroupFileError,
  saveBlobAs,
} from "../lib/api/groupFiles";
import type { GroupFile } from "../lib/api/groupFiles";
import * as olm from "../lib/crypto/olm-lite";
import {
  SHARE_FRAMERATES,
  SHARE_RESOLUTIONS,
  shareQuality,
} from "../lib/rtc/callSession";
import type { ShareQuality } from "../lib/rtc/callSession";
import { callStatusText } from "../lib/rtc/callStatus";
import { useAuthStore } from "../stores/auth";
import { useCallStore } from "../stores/call";
import { useFriendsStore } from "../stores/friends";
import { useGroupsStore } from "../stores/groups";
import { useProfileStore } from "../stores/profile";
import { RECALL_WINDOW_MS, useWsStore } from "../stores/ws";
import type { ChatMessage, ChatMessageMedia, Conversation } from "../stores/ws";

const { t, locale } = useI18n();
const router = useRouter();
const route = useRoute();
const auth = useAuthStore();
const ws = useWsStore();
const friends = useFriendsStore();
const groups = useGroupsStore();
const profile = useProfileStore();
const calls = useCallStore();
const { preference: themePreference } = useTheme();
/** Own instance, captured for cross-breakpoint DOM measurement at runtime. */
const viewInstance = getCurrentInstance();

const PREVIEW_MAX_CHARS = 40;
/** Group name bounds (mirrors the server contract: 1..32). */
const GROUP_NAME_MAX = 32;
/** Audio filename caption truncation (chars before the ellipsis). */
const AUDIO_FILE_NAME_MAX = 24;

const newPeerUsername = ref("");
/** M3: kind chosen in the segmented 普通/密聊 control. */
const newConversationKind = ref<"direct" | "secret">("direct");
const creatingConversation = ref(false);
const conversationError = ref("");
const draft = ref("");
/** Local half of the safety code for the active secret thread (or null). */
const sasCode = ref<string | null>(null);
const composerEl = ref<HTMLTextAreaElement | null>(null);
const messagesEndRef = ref<HTMLElement | null>(null);

// --- M11 groups: create dialog state ------------------------------------
const groupDialogOpen = ref(false);
const groupNameInput = ref("");
/** Trivial client-side friend filter for the create dialog. */
const groupMemberFilter = ref("");
/** Usernames of the friends selected in the create dialog. */
const groupSelected = ref<string[]>([]);
const creatingGroup = ref(false);
const groupCreateError = ref("");

// --- M11 groups: info panel state ---------------------------------------
const groupPanelOpen = ref(false);
const groupInviteUsername = ref("");
const groupActionError = ref("");
/** user_id of the member whose action is in flight (disables its row). */
const busyMemberId = ref<string | null>(null);
/** Pending destructive action awaiting an in-app confirmation. */
const groupConfirm = ref<{
  type: "kick" | "role" | "transfer" | "leave";
  userId?: string;
  role?: "admin" | "member";
  message: string;
} | null>(null);

// --- M12b app settings dialog -------------------------------------------
const settingsOpen = ref(false);

// --- M12b group settings: description, avatar, member titles ------------
const groupDescriptionEditing = ref(false);
const groupDescriptionDraft = ref("");
const groupDescriptionSaving = ref(false);
const groupAvatarInputEl = ref<HTMLInputElement | null>(null);
/** Member whose custom title is being edited; null = dialog closed. */
const titleDialogMember = ref<GroupMember | null>(null);
const titleDraft = ref("");
const titleSaving = ref(false);

// --- M14 calls: incoming/active panel + screen-share quality picker ------
const sharePickerOpen = ref(false);
/** Selected capture resolution label (matches SHARE_RESOLUTIONS labels). */
const shareResolutionLabel = ref("720p");
/** Selected capture framerate (matches SHARE_FRAMERATES). */
const shareFrameRate = ref(30);
/** Ticking clock driving the call duration display. */
const callNow = ref(Date.now());
let callTimer: ReturnType<typeof setInterval> | null = null;
let callToastTimer: ReturnType<typeof setTimeout> | null = null;
/** Video sinks bound imperatively (srcObject is a DOM property, not an attr). */
const remoteVideoEl = ref<HTMLVideoElement | null>(null);
const selfVideoEl = ref<HTMLVideoElement | null>(null);
const selfThumbEl = ref<HTMLVideoElement | null>(null);

/** Call affordances exist in direct + group threads, never in secret chats. */
const callsEnabled = computed(
  () =>
    ws.activeConversation !== null && ws.activeConversation.kind !== "secret",
);
/** Curated caller label for the incoming dialog. */
const incomingCallerName = computed(() => {
  const peer = calls.peer;
  if (peer === null) return "";
  const display = peer.displayName?.trim() ?? "";
  return display.length > 0 ? display : peer.username;
});
/** mm:ss duration anchored at the moment the call first went live. */
const callDuration = computed(() => {
  if (calls.startedAt === null) return "00:00";
  const total = Math.max(
    0,
    Math.floor((callNow.value - calls.startedAt) / 1000),
  );
  const mm = String(Math.floor(total / 60)).padStart(2, "0");
  const ss = String(total % 60).padStart(2, "0");
  return `${mm}:${ss}`;
});
/** The quality the picker currently represents. */
const selectedShareQuality = computed<ShareQuality>(() =>
  shareQuality(shareResolutionLabel.value, shareFrameRate.value),
);
/** Human status line for the active call panel (never empty — see helper). */
const callStatusLabel = computed(() =>
  callStatusText(
    calls.status,
    (key, params) => (params === undefined ? t(key) : t(key, params)),
    {
      isGroup: calls.isGroup,
      participantCount: calls.participants.length,
    },
  ),
);
/** Avatar ring order: group roster, or [self, peer] for a 1:1 call. */
const callParticipantIds = computed<string[]>(() => {
  if (calls.isGroup) return calls.participants.map((p) => p.user_id);
  const ids: string[] = [];
  if (calls.myUserId() !== "") ids.push(calls.myUserId());
  if (calls.peer !== null) ids.push(calls.peer.userId);
  return ids;
});
/** True when a remote screen takes the big stage (else the self preview does). */
const stageIsRemote = computed(() => calls.videoStageUserId !== null);

// --- M13b group files: usage, listing, up/download/delete, drag & drop ---
const fileUploading = ref(false);
const fileUploadPercent = ref(0);
const fileError = ref("");
/** file_id whose download/delete is in flight (disables its row actions). */
const fileBusyId = ref<string | null>(null);
/** File awaiting delete confirmation; null = dialog closed. */
const fileDeleteTarget = ref<GroupFile | null>(null);
const groupFileInputEl = ref<HTMLInputElement | null>(null);
const fileDragActive = ref(false);
let fileDragDepth = 0;

// --- M8 media / emoji composer state -----------------------------------
const emojiOpen = ref(false);
const uploading = ref(false);
const uploadPercent = ref(0);
const uploadError = ref("");
/** Media shown full-screen in the lightbox; null when closed. */
const lightboxMedia = ref<ChatMessageMedia | null>(null);
const imageInputEl = ref<HTMLInputElement | null>(null);
const videoInputEl = ref<HTMLInputElement | null>(null);
const audioInputEl = ref<HTMLInputElement | null>(null);

// --- M9 voice messages: recorder + custom minimal audio player ----------
const recording = ref(false);
const recordingSeconds = ref(0);
let mediaRecorder: MediaRecorder | null = null;
let recordedChunks: Blob[] = [];
let recordingTimer: ReturnType<typeof setInterval> | null = null;
let recordingStartedAt = 0;
let recordingCancelled = false;
let recordingStream: MediaStream | null = null;

/** Live recording clock as mm:ss. */
const voiceTimer = computed(() => {
  const total = recordingSeconds.value;
  const mm = String(Math.floor(total / 60)).padStart(2, "0");
  const ss = String(total % 60).padStart(2, "0");
  return `${mm}:${ss}`;
});

/** mediaId of the audio bubble currently playing (null = idle). */
const playingMediaId = ref<string | null>(null);
/** 0–1 playback progress of the active voice bubble. */
const audioProgress = ref(0);
/** Single lazily-created element; starting another pauses the previous. */
let audioEl: HTMLAudioElement | null = null;

/** Secret chats are e2ee-only: media attachments are not offered there. */
const mediaEnabled = computed(
  () =>
    ws.activeConversation !== null && ws.activeConversation.kind !== "secret",
);

onMounted(() => {
  // Router guards ensure /chat is only reachable while authed; connecting
  // here keeps tests (anon auth) free of network side effects.
  if (auth.status === "authed") {
    profile.ensureLoaded();
    void ws.connect();
  }
  document.addEventListener("keydown", onGlobalKeydown);
  window.addEventListener("resize", onSessionListResize);
  syncSessionIndicator();
});

onBeforeUnmount(() => {
  document.removeEventListener("keydown", onGlobalKeydown);
  window.removeEventListener("resize", onSessionListResize);
  // Release the microphone and any playing audio element cleanly.
  if (recording.value && mediaRecorder !== null) {
    recordingCancelled = true;
    try {
      mediaRecorder.stop();
    } catch {
      // Recorder already stopped — nothing to release.
    }
  }
  resetRecordingState();
  pauseAudio();
  // Call UI timers are local to this view; the call itself stays in the store.
  if (callTimer !== null) {
    clearInterval(callTimer);
    callTimer = null;
  }
  if (callToastTimer !== null) {
    clearTimeout(callToastTimer);
    callToastTimer = null;
  }
});

watch(
  () => [ws.activeConversationId, ws.activeMessages.length] as const,
  async () => {
    await nextTick();
    // Optional call: jsdom (tests) does not implement scrollIntoView.
    messagesEndRef.value?.scrollIntoView?.({ block: "end" });
  },
);

// --- M14 call view glue: duration clock, toast auto-dismiss, video sinks --
watch(
  () => calls.isInCall,
  (inCall) => {
    if (inCall) {
      callNow.value = Date.now();
      if (callTimer === null) {
        callTimer = setInterval(() => {
          callNow.value = Date.now();
        }, 1_000);
      }
    } else {
      if (callTimer !== null) {
        clearInterval(callTimer);
        callTimer = null;
      }
      sharePickerOpen.value = false;
    }
  },
);

watch(
  () => calls.toast,
  (toast) => {
    if (callToastTimer !== null) {
      clearTimeout(callToastTimer);
      callToastTimer = null;
    }
    if (toast !== null) {
      callToastTimer = setTimeout(() => {
        callToastTimer = null;
        calls.clearToast();
      }, 4_000);
    }
  },
);

// Post-flush: the <video> sinks only exist while the stage renders.
watch(
  [() => calls.videoStageUserId, () => calls.remoteStreams, remoteVideoEl],
  () => {
    const userId = calls.videoStageUserId;
    const stream =
      userId !== null ? (calls.remoteStreams[userId] ?? null) : null;
    if (remoteVideoEl.value !== null) remoteVideoEl.value.srcObject = stream;
  },
  { flush: "post" },
);
watch(
  [() => calls.localScreenStream, selfVideoEl, selfThumbEl],
  () => {
    const stream = calls.localScreenStream;
    if (selfVideoEl.value !== null) selfVideoEl.value.srcObject = stream;
    if (selfThumbEl.value !== null) selfThumbEl.value.srcObject = stream;
  },
  { flush: "post" },
);

async function logout(): Promise<void> {
  ws.dispose();
  auth.logout();
  await router.push("/login");
}

// ---------------------------------------------------------------------
// Sessions pane
// ---------------------------------------------------------------------

/** Peer messages label with the peer's name; falls back to the raw id. */
function senderLabel(message: { senderId: string }): string {
  const conv = ws.activeConversation;
  if (conv !== null && conv.kind === "group") {
    // Group bubbles are attributed to the actual member (never "peer").
    return (
      groups.memberNames[conv.conversationId]?.[message.senderId] ??
      message.senderId
    );
  }
  if (
    conv !== null &&
    message.senderId === conv.peerUserId &&
    conv.peerUsername.length > 0
  ) {
    return peerLabel(conv);
  }
  return message.senderId;
}

/** Avatar emoji of a group member in the active conversation (or null). */
function groupAvatarOf(userId: string): string | null {
  const id = ws.activeConversationId;
  if (id === null) return null;
  return groups.memberAvatars[id]?.[userId] ?? null;
}

/** Best display label for a conversation's peer (display name when known). */
function peerLabel(conversation: Conversation): string {
  if (conversation.kind === "group") {
    const name = conversation.name?.trim() ?? "";
    return name.length > 0 ? name : t("group.groupUnknown");
  }
  if (conversation.peerDisplayName?.trim().length) {
    return conversation.peerDisplayName;
  }
  return conversation.peerUsername.length > 0
    ? conversation.peerUsername
    : t("chat.peerUnknown");
}

/** Navigate to a peer's profile, seeding the transient view from the row. */
function openPeerProfile(conversation: Conversation): void {
  // Groups have no peer profile; the avatar opens the group info panel.
  if (conversation.kind === "group") {
    openSessionAvatar(conversation);
    return;
  }
  profile.seedPeer(conversation.peerUserId, {
    username: conversation.peerUsername,
    uid: conversation.peerUid,
    displayName: conversation.peerDisplayName,
    avatar: conversation.peerAvatar ?? null,
  });
  void router.push(`/profile/${encodeURIComponent(conversation.peerUserId)}`);
}

/** Session-row avatar click: peer profile for direct, info panel for groups. */
function openSessionAvatar(conversation: Conversation): void {
  if (conversation.kind === "group") {
    if (ws.activeConversationId !== conversation.conversationId) {
      ws.openConversation(conversation.conversationId);
    }
    openGroupPanel();
    return;
  }
  openPeerProfile(conversation);
}

/** Navigate to my own profile (nav avatar). */
function openSelfProfile(): void {
  void router.push("/profile");
}

function truncatePreview(body: string | null): string {
  if (body === null) return "";
  return body.length > PREVIEW_MAX_CHARS
    ? `${body.slice(0, PREVIEW_MAX_CHARS)}…`
    : body;
}

/**
 * Session-row preview text. Media messages carry an empty body, so their
 * last-known kind renders a localized placeholder instead.
 */
function conversationPreview(conversation: Conversation): string {
  if (conversation.lastMessageKind === "image") return t("chat.previewImage");
  if (conversation.lastMessageKind === "video") return t("chat.previewVideo");
  if (conversation.lastMessageKind === "audio") return t("chat.previewAudio");
  return truncatePreview(conversation.lastMessagePreview);
}

const relativeFormat = computed(
  () => new Intl.RelativeTimeFormat(locale.value, { numeric: "auto" }),
);

function relativeTime(iso: string): string {
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return "";
  const diffSecs = Math.round((then - Date.now()) / 1000);
  const absSecs = Math.abs(diffSecs);
  if (absSecs < 60) return relativeFormat.value.format(diffSecs, "second");
  if (absSecs < 3600) {
    return relativeFormat.value.format(Math.round(diffSecs / 60), "minute");
  }
  if (absSecs < 86_400) {
    return relativeFormat.value.format(Math.round(diffSecs / 3600), "hour");
  }
  if (absSecs < 7 * 86_400) {
    return relativeFormat.value.format(Math.round(diffSecs / 86_400), "day");
  }
  return new Intl.DateTimeFormat(locale.value, {
    month: "short",
    day: "numeric",
  }).format(new Date(then));
}

async function submitNewConversation(): Promise<void> {
  const peer = newPeerUsername.value.trim();
  if (peer.length === 0 || creatingConversation.value) return;
  creatingConversation.value = true;
  conversationError.value = "";
  try {
    await ws.createOrOpenConversation(peer, newConversationKind.value);
    newPeerUsername.value = "";
    newConversationKind.value = "direct";
  } catch (error) {
    // PeerNotReadyError (secret peer never published keys) and
    // NoOneTimeKeysError are localized inside apiErrorMessage.
    conversationError.value = apiErrorMessage(error, (key) => t(key));
  } finally {
    creatingConversation.value = false;
  }
}

// ---------------------------------------------------------------------
// M3 secret chats: safety code for the active thread
// ---------------------------------------------------------------------

watch(
  () => [ws.activeConversationId, ws.activeConversation?.kind] as const,
  async () => {
    sasCode.value = null;
    const conversation = ws.activeConversation;
    if (conversation?.kind !== "secret") return;
    const peerIdentity = olm.getPeerIdentity(conversation.conversationId);
    if (peerIdentity === null) return;
    try {
      sasCode.value = await olm.safetyCode(peerIdentity);
    } catch {
      sasCode.value = null;
    }
  },
  { immediate: true },
);

// M11: fetch authoritative group info (roster + my role) when a group thread
// becomes active, so the header member count and bubble identity are fresh.
watch(
  () => [ws.activeConversationId, ws.activeConversation?.kind] as const,
  ([id, kind]) => {
    if (id === null || kind !== "group") {
      groupPanelOpen.value = false;
      groupDescriptionEditing.value = false;
      titleDialogMember.value = null;
      groups.unhostFilesView();
      return;
    }
    void groups.fetchInfo(id);
  },
  { immediate: true },
);

function openConversation(conversationId: number): void {
  // Resets unread and advances lastSeenSeq inside the store.
  ws.openConversation(conversationId);
}

// ---------------------------------------------------------------------
// Sessions: sliding active-conversation indicator
// ---------------------------------------------------------------------

/**
 * Position of the grey pill behind the active session row, relative to the
 * session list. `null` while nothing is active (the pill stays hidden).
 */
const indicatorMetrics = ref<{ y: number; height: number } | null>(null);
/** Flipped on after the first placement so the pill only glides on changes. */
const indicatorSettled = ref(false);

const indicatorStyle = computed(() => {
  const metrics = indicatorMetrics.value;
  if (metrics === null) return { opacity: "0" };
  return {
    transform: `translateY(${metrics.y}px)`,
    height: `${metrics.height}px`,
  };
});

const indicatorTransitionClass = computed(() =>
  indicatorSettled.value
    ? "transition-transform duration-[280ms] ease-[cubic-bezier(0.34,1.3,0.64,1)] motion-reduce:transition-none"
    : "",
);

/** The shell renders one session list per breakpoint; measure the visible one. */
function visibleSessionList(root: HTMLElement): HTMLElement | null {
  const lists = Array.from(
    root.querySelectorAll<HTMLElement>('[data-testid="session-list"]'),
  );
  return (
    lists.find(
      (list) =>
        list.offsetParent !== null && list.getBoundingClientRect().width > 0,
    ) ??
    lists[0] ??
    null
  );
}

/** Scrolls the active row into view when the sessions pane is scrolled away. */
function revealSessionRow(row: HTMLElement): void {
  const scroller = row.closest<HTMLElement>('[data-testid^="shell-sessions"]');
  if (scroller === null) return;
  const rowRect = row.getBoundingClientRect();
  const scrollRect = scroller.getBoundingClientRect();
  const fullyVisible =
    rowRect.top >= scrollRect.top && rowRect.bottom <= scrollRect.bottom;
  // Optional call: jsdom (tests) does not implement scrollIntoView.
  if (!fullyVisible) row.scrollIntoView?.({ block: "nearest" });
}

/**
 * Re-measures the active session row and glides the pill to it. Safe to call
 * at any time (mount / active change / list reorder / viewport resize).
 */
function syncSessionIndicator(): void {
  const root: unknown = viewInstance?.proxy?.$el;
  if (!(root instanceof HTMLElement)) return;
  const list = visibleSessionList(root);
  const activeId = ws.activeConversationId;
  if (list === null || activeId === null) {
    indicatorMetrics.value = null;
    return;
  }
  const row = list.querySelector<HTMLElement>(
    `[data-conversation-id="${activeId}"]`,
  );
  if (row === null) {
    indicatorMetrics.value = null;
    return;
  }
  revealSessionRow(row);
  const firstPlacement = indicatorMetrics.value === null;
  indicatorMetrics.value = { y: row.offsetTop, height: row.offsetHeight };
  if (firstPlacement) {
    // Jump on first placement; enable the glide from the next change on.
    indicatorSettled.value = false;
    requestAnimationFrame(() => {
      indicatorSettled.value = true;
    });
  }
}

function onSessionListResize(): void {
  syncSessionIndicator();
}

// Re-measure when the active conversation changes or the list reorders /
// changes membership (new message bumps, deletions, new conversations).
watch(
  () => [
    ws.activeConversationId,
    ws.sortedConversations.map((c) => c.conversationId).join(","),
  ],
  () => {
    void nextTick(syncSessionIndicator);
  },
);

// ---------------------------------------------------------------------
// Message thread
// ---------------------------------------------------------------------

function dayKey(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "unknown";
  return `${date.getFullYear()}-${date.getMonth() + 1}-${date.getDate()}`;
}

function dayLabel(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "";
  const key = dayKey(iso);
  if (key === dayKey(new Date().toISOString())) return t("chat.dateToday");
  if (key === dayKey(new Date(Date.now() - 86_400_000).toISOString())) {
    return t("chat.dateYesterday");
  }
  return new Intl.DateTimeFormat(locale.value, {
    year: "numeric",
    month: "long",
    day: "numeric",
  }).format(date);
}

interface DayGroup {
  key: string;
  label: string;
  messages: ChatMessage[];
}

const dayGroups = computed<DayGroup[]>(() => {
  const groups: DayGroup[] = [];
  for (const message of ws.activeMessages) {
    const key = dayKey(message.sentAt);
    const last = groups[groups.length - 1];
    if (last !== undefined && last.key === key) {
      last.messages.push(message);
    } else {
      groups.push({
        key,
        label: dayLabel(message.sentAt),
        messages: [message],
      });
    }
  }
  return groups;
});

function clockTime(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "";
  return new Intl.DateTimeFormat(locale.value, {
    hour: "2-digit",
    minute: "2-digit",
  }).format(date);
}

// ---------------------------------------------------------------------
// Composer
// ---------------------------------------------------------------------

function autoGrow(): void {
  const el = composerEl.value;
  if (el === null) return;
  el.style.height = "auto";
  el.style.height = `${Math.min(el.scrollHeight, 160)}px`;
}

function sendMessage(): void {
  const conversationId = ws.activeConversationId;
  if (conversationId === null || !ws.isConnected) return;
  const body = draft.value.trim();
  if (body.length === 0) return;
  ws.notifyTypingStop(conversationId);
  ws.send(conversationId, body);
  draft.value = "";
  autoGrow();
}

function retryMessage(clientMsgId: string): void {
  ws.retry(clientMsgId);
}

// ---------------------------------------------------------------------
// M2: reply / forward / recall / typing
// ---------------------------------------------------------------------

/** Forward picker source message; null = picker closed. */
const forwardSource = ref<ChatMessage | null>(null);

/** Picker lists every OTHER conversation (no self-target). */
const forwardTargets = computed(() =>
  ws.sortedConversations.filter(
    (c) => c.conversationId !== forwardSource.value?.conversationId,
  ),
);

/** Sender-only, 120s window (mirrors the server-side RecallPolicy). */
function canRecall(message: ChatMessage): boolean {
  if (!message.mine || message.recalled || message.messageId === null) {
    return false;
  }
  const sentAt = Date.parse(message.sentAt);
  return Number.isFinite(sentAt) && Date.now() - sentAt < RECALL_WINDOW_MS;
}

function replyToMessage(message: ChatMessage): void {
  ws.setReplyContext(message);
}

function cancelReply(): void {
  ws.clearReplyContext();
}

function openForwardPicker(message: ChatMessage): void {
  if (!message.recalled) forwardSource.value = message;
}

function confirmForward(conversationId: number): void {
  const source = forwardSource.value;
  forwardSource.value = null;
  if (source === null) return;
  ws.forwardMessage(conversationId, source);
}

function recallMessage(message: ChatMessage): void {
  if (canRecall(message)) ws.recallMessage(message.conversationId, message);
}

function onComposerInput(): void {
  autoGrow();
  const conversationId = ws.activeConversationId;
  if (conversationId !== null && ws.isConnected) {
    ws.notifyTypingStart(conversationId);
  }
}

function onComposerBlur(): void {
  const conversationId = ws.activeConversationId;
  if (conversationId !== null) ws.notifyTypingStop(conversationId);
}

// ---------------------------------------------------------------------
// M8: emoji panel, media attach + upload, media rendering
// ---------------------------------------------------------------------

/** Public URL of an uploaded attachment (relative in web dev, absolute in Tauri). */
function mediaUrl(media: ChatMessageMedia): string {
  return `${apiBase()}/api/media/${encodeURIComponent(media.mediaId)}`;
}

function toggleEmoji(): void {
  uploadError.value = "";
  emojiOpen.value = !emojiOpen.value;
}

/** Appends an emoji to the end of the draft and keeps the panel open. */
function insertEmoji(emoji: string): void {
  draft.value += emoji;
  emojiOpen.value = true;
  void nextTick(() => {
    autoGrow();
    composerEl.value?.focus();
  });
}

function openLightbox(media: ChatMessageMedia): void {
  if (media.kind === "image") lightboxMedia.value = media;
}

function closeLightbox(): void {
  lightboxMedia.value = null;
}

/** Esc closes transient overlays regardless of focus. */
function onGlobalKeydown(event: KeyboardEvent): void {
  if (event.key !== "Escape") return;
  emojiOpen.value = false;
  lightboxMedia.value = null;
  settingsOpen.value = false;
  titleDialogMember.value = null;
}

function pickImage(): void {
  uploadError.value = "";
  imageInputEl.value?.click();
}

function pickVideo(): void {
  uploadError.value = "";
  videoInputEl.value?.click();
}

function pickAudio(): void {
  uploadError.value = "";
  audioInputEl.value?.click();
}

function onFilePicked(event: Event): void {
  const input = event.target as HTMLInputElement;
  const file = input.files?.[0] ?? null;
  // Reset so picking the same file twice still fires `change`.
  input.value = "";
  if (file !== null) void handleMediaFile(file);
}

function probeImageSize(
  url: string,
): Promise<{ width: number; height: number } | null> {
  return new Promise((resolve) => {
    const img = new Image();
    img.onload = () =>
      resolve(
        img.naturalWidth > 0 && img.naturalHeight > 0
          ? { width: img.naturalWidth, height: img.naturalHeight }
          : null,
      );
    img.onerror = () => resolve(null);
    img.src = url;
  });
}

function probeVideoSize(
  url: string,
): Promise<{ width: number; height: number } | null> {
  return new Promise((resolve) => {
    const video = document.createElement("video");
    video.preload = "metadata";
    video.onloadedmetadata = () =>
      resolve(
        video.videoWidth > 0 && video.videoHeight > 0
          ? { width: video.videoWidth, height: video.videoHeight }
          : null,
      );
    video.onerror = () => resolve(null);
    video.src = url;
  });
}

/** Best-effort audio duration (mp3/m4a/wav/…) for the voice bubble caption. */
function probeAudioDuration(url: string): Promise<number | null> {
  return new Promise((resolve) => {
    const audio = document.createElement("audio");
    audio.preload = "metadata";
    audio.onloadedmetadata = () =>
      resolve(
        Number.isFinite(audio.duration) && audio.duration > 0
          ? Math.round(audio.duration * 1000)
          : null,
      );
    audio.onerror = () => resolve(null);
    audio.src = url;
  });
}

/** Best-effort natural dimensions (jsdom/offline failures return {}). */
async function probeDimensions(
  file: File,
  kind: MediaKind,
): Promise<{ width?: number; height?: number }> {
  try {
    if (
      typeof URL === "undefined" ||
      typeof URL.createObjectURL !== "function"
    ) {
      return {};
    }
    if (kind === "audio") return {};
    const url = URL.createObjectURL(file);
    try {
      const size =
        kind === "image"
          ? await probeImageSize(url)
          : await probeVideoSize(url);
      return size ?? {};
    } finally {
      URL.revokeObjectURL(url);
    }
  } catch {
    return {};
  }
}

function mediaErrorMessage(error: unknown, file: File): string {
  if (error instanceof MediaUploadError) {
    if (error.code === "too_large") {
      const kind = checkMediaFile(file).kind;
      const limit = kind !== null ? formatBytes(maxBytesForKind(kind)) : "";
      return t("chat.mediaTooLarge", { limit });
    }
    if (error.code === "unsupported_type") {
      return t("chat.mediaUnsupportedType");
    }
  }
  return t("chat.mediaUploadFailed");
}

/**
 * Full media flow: client-side size/type check → upload original bytes with
 * progress → best-effort dimension probe → optimistic ws send.
 */
async function handleMediaFile(file: File): Promise<void> {
  const conversationId = ws.activeConversationId;
  if (conversationId === null || !ws.isConnected || uploading.value) return;

  const check = checkMediaFile(file);
  if (check.kind === null || check.error === "unsupported_type") {
    uploadError.value = t("chat.mediaUnsupportedType");
    return;
  }
  if (check.error === "too_large") {
    uploadError.value = t("chat.mediaTooLarge", {
      limit: formatBytes(maxBytesForKind(check.kind)),
    });
    return;
  }

  const token = await auth.ensureAccessToken();
  if (token === null) {
    uploadError.value = t("chat.mediaUploadFailed");
    return;
  }

  uploadError.value = "";
  uploading.value = true;
  uploadPercent.value = 0;
  try {
    const result = await uploadMedia(token, file, (percent) => {
      uploadPercent.value = percent;
    });
    const dimensions = await probeDimensions(file, result.kind);
    let durationMs: number | null = null;
    if (result.kind === "audio") {
      try {
        if (
          typeof URL !== "undefined" &&
          typeof URL.createObjectURL === "function"
        ) {
          const url = URL.createObjectURL(file);
          try {
            durationMs = await probeAudioDuration(url);
          } finally {
            URL.revokeObjectURL(url);
          }
        }
      } catch {
        durationMs = null;
      }
    }
    const media: ChatMessageMedia = {
      mediaId: result.media_id,
      kind: result.kind,
      mime: result.mime,
      bytes: result.bytes,
      fileName: result.file_name,
      ...dimensions,
      ...(durationMs !== null ? { durationMs } : {}),
    };
    ws.sendMedia(conversationId, media);
  } catch (error) {
    uploadError.value = mediaErrorMessage(error, file);
  } finally {
    uploading.value = false;
    uploadPercent.value = 0;
  }
}

// ---------------------------------------------------------------------
// M9 voice messages: MediaRecorder capture + custom minimal player
// ---------------------------------------------------------------------

/** Opus encode target: 128 kbps is transparent for speech (default ~32k). */
const VOICE_BITS_PER_SECOND = 128_000;

/** Preferred recorder container: opus/webm when supported, else defaults. */
function pickAudioMime(): string | null {
  if (typeof MediaRecorder === "undefined") return null;
  const candidates = [
    "audio/webm;codecs=opus",
    "audio/webm",
    "audio/ogg;codecs=opus",
    "audio/mp4",
  ];
  if (typeof MediaRecorder.isTypeSupported !== "function") return null;
  for (const candidate of candidates) {
    if (MediaRecorder.isTypeSupported(candidate)) return candidate;
  }
  return null;
}

function clearRecordingTimer(): void {
  if (recordingTimer !== null) {
    clearInterval(recordingTimer);
    recordingTimer = null;
  }
}

function stopRecordingTracks(): void {
  recordingStream?.getTracks().forEach((track) => track.stop());
  recordingStream = null;
}

/** Returns the composer to an idle state; safe to call from any path. */
function resetRecordingState(): void {
  clearRecordingTimer();
  stopRecordingTracks();
  mediaRecorder = null;
  recordedChunks = [];
  recordingCancelled = false;
  recording.value = false;
  recordingSeconds.value = 0;
}

function voiceErrorMessage(error: unknown): string {
  if (error instanceof MediaUploadError) {
    if (error.code === "too_large") {
      return t("chat.mediaTooLarge", {
        limit: formatBytes(maxBytesForKind("audio")),
      });
    }
    if (error.code === "unsupported_type") {
      return t("chat.mediaUnsupportedType");
    }
  }
  return t("chat.voiceUploadFailed");
}

async function startVoiceRecording(): Promise<void> {
  const conversationId = ws.activeConversationId;
  if (
    recording.value ||
    uploading.value ||
    conversationId === null ||
    !ws.isConnected ||
    !mediaEnabled.value
  ) {
    return;
  }
  if (
    typeof MediaRecorder === "undefined" ||
    typeof navigator === "undefined" ||
    navigator.mediaDevices?.getUserMedia === undefined
  ) {
    uploadError.value = t("chat.voicePermissionDenied");
    return;
  }
  uploadError.value = "";
  let stream: MediaStream;
  try {
    // 48 kHz mono capture; the clarity lever is the encoder bitrate below
    // (Chromium's default ~32 kbps opus is noticeably muffled).
    stream = await navigator.mediaDevices.getUserMedia({
      audio: {
        sampleRate: { ideal: 48000 },
        channelCount: { ideal: 1 },
      },
    });
  } catch {
    // Permission denied / no device: surface a localized error and reset.
    uploadError.value = t("chat.voicePermissionDenied");
    resetRecordingState();
    return;
  }
  recordingStream = stream;
  recordedChunks = [];
  recordingCancelled = false;

  const mime = pickAudioMime();
  let recorder: MediaRecorder;
  try {
    recorder =
      mime !== null
        ? new MediaRecorder(stream, {
            mimeType: mime,
            audioBitsPerSecond: VOICE_BITS_PER_SECOND,
          })
        : new MediaRecorder(stream, {
            audioBitsPerSecond: VOICE_BITS_PER_SECOND,
          });
  } catch {
    try {
      recorder = new MediaRecorder(stream);
    } catch {
      uploadError.value = t("chat.voiceUploadFailed");
      resetRecordingState();
      return;
    }
  }
  mediaRecorder = recorder;
  recorder.ondataavailable = (event: BlobEvent) => {
    if (event.data !== undefined && event.data.size > 0) {
      recordedChunks.push(event.data);
    }
  };
  recorder.onstop = () => {
    void finishVoiceRecording(conversationId);
  };
  recorder.onerror = () => {
    uploadError.value = t("chat.voiceUploadFailed");
    resetRecordingState();
  };

  recordingStartedAt = Date.now();
  recordingSeconds.value = 0;
  recording.value = true;
  clearRecordingTimer();
  recordingTimer = setInterval(() => {
    recordingSeconds.value = Math.floor(
      (Date.now() - recordingStartedAt) / 1000,
    );
  }, 250);
  try {
    recorder.start();
  } catch {
    uploadError.value = t("chat.voiceUploadFailed");
    resetRecordingState();
  }
}

function stopVoiceRecording(): void {
  if (!recording.value || mediaRecorder === null) return;
  if (mediaRecorder.state !== "inactive") {
    mediaRecorder.stop();
  } else {
    resetRecordingState();
  }
}

function cancelVoiceRecording(): void {
  if (!recording.value) return;
  recordingCancelled = true;
  if (mediaRecorder !== null && mediaRecorder.state !== "inactive") {
    mediaRecorder.stop();
  } else {
    resetRecordingState();
  }
}

/** onstop → upload the recorded blob verbatim and send an audio message. */
async function finishVoiceRecording(conversationId: number): Promise<void> {
  const durationMs = Math.max(0, Date.now() - recordingStartedAt);
  const chunks = recordedChunks;
  const cancelled = recordingCancelled;
  const recorderMime = mediaRecorder?.mimeType ?? "";
  resetRecordingState();

  if (cancelled || chunks.length === 0) return;

  const containerMime =
    recorderMime.split(";")[0]?.trim() !== ""
      ? (recorderMime.split(";")[0]?.trim() ?? "audio/webm")
      : "audio/webm";
  const blob = new Blob(chunks, { type: containerMime });
  const ext = containerMime.includes("ogg")
    ? "ogg"
    : containerMime.includes("mp4")
      ? "m4a"
      : "webm";
  const file = new File([blob], `voice-${Date.now()}.${ext}`, {
    type: containerMime,
  });

  uploading.value = true;
  uploadPercent.value = 0;
  try {
    const token = await auth.ensureAccessToken();
    if (token === null) {
      uploadError.value = t("chat.voiceUploadFailed");
      return;
    }
    const result = await uploadMedia(token, file, (percent) => {
      uploadPercent.value = percent;
    });
    const media: ChatMessageMedia = {
      mediaId: result.media_id,
      kind: "audio",
      mime: result.mime,
      bytes: result.bytes,
      fileName: result.file_name,
      durationMs,
    };
    ws.sendMedia(conversationId, media);
  } catch (error) {
    uploadError.value = voiceErrorMessage(error);
  } finally {
    uploading.value = false;
    uploadPercent.value = 0;
  }
}

/** mm:ss label for a stored voice duration (0 when unknown). */
function formatDuration(durationMs: number | undefined): string {
  const totalSecs = Math.max(0, Math.round((durationMs ?? 0) / 1000));
  const mm = String(Math.floor(totalSecs / 60)).padStart(2, "0");
  const ss = String(totalSecs % 60).padStart(2, "0");
  return `${mm}:${ss}`;
}

/**
 * File-name caption above an audio player. Uploaded audio files show their
 * (truncated) name; generated recorder names (`voice-….webm`) are hidden —
 * they carry no information and would only add noise.
 */
function audioFileName(media: ChatMessageMedia): string | null {
  const name = media.fileName?.trim() ?? "";
  if (name.length === 0 || name.startsWith("voice-")) return null;
  return name.length > AUDIO_FILE_NAME_MAX
    ? `${name.slice(0, AUDIO_FILE_NAME_MAX)}…`
    : name;
}

/** Pauses and rewinds the shared audio element, clearing player state. */
function pauseAudio(): void {
  if (audioEl !== null) {
    try {
      audioEl.pause();
      audioEl.currentTime = 0;
    } catch {
      // jsdom / detached element: nothing to stop.
    }
  }
  playingMediaId.value = null;
  audioProgress.value = 0;
}

/** Play/pause one voice bubble; starting another pauses the previous one. */
function toggleAudio(media: ChatMessageMedia): void {
  if (playingMediaId.value === media.mediaId) {
    pauseAudio();
    return;
  }
  if (audioEl !== null) {
    try {
      audioEl.pause();
      audioEl.currentTime = 0;
    } catch {
      // Detached element — replace it below.
    }
  }
  if (typeof Audio === "undefined") return;
  const el = new Audio(mediaUrl(media));
  audioEl = el;
  playingMediaId.value = media.mediaId;
  audioProgress.value = 0;
  el.addEventListener("timeupdate", () => {
    const fallback = (media.durationMs ?? 0) / 1000;
    const dur =
      Number.isFinite(el.duration) && el.duration > 0 ? el.duration : fallback;
    audioProgress.value = dur > 0 ? Math.min(1, el.currentTime / dur) : 0;
  });
  el.addEventListener("ended", () => {
    playingMediaId.value = null;
    audioProgress.value = 0;
  });
  el.addEventListener("error", () => {
    playingMediaId.value = null;
    audioProgress.value = 0;
  });
  try {
    const played = el.play();
    if (played !== undefined && typeof played.catch === "function") {
      played.catch(() => {
        playingMediaId.value = null;
      });
    }
  } catch {
    playingMediaId.value = null;
  }
}

// ---------------------------------------------------------------------
// M11 groups: create dialog, info panel, membership actions
// ---------------------------------------------------------------------

const myUserId = computed<string>(() => auth.user?.userId ?? "");

/** Authoritative group info for the open thread (null when not a group). */
const activeGroupInfo = computed<GroupInfo | null>(() =>
  ws.activeConversation?.kind === "group"
    ? groups.infoFor(ws.activeConversationId)
    : null,
);

/** Current member's role in the active group ("member" until info loads). */
const myGroupRole = computed<GroupRole>(
  () => activeGroupInfo.value?.my_role ?? "member",
);

/** Group files state for the active thread (null before the first fetch). */
const activeGroupFiles = computed(() =>
  groups.filesFor(ws.activeConversationId),
);
const groupFileList = computed<GroupFile[]>(
  () => activeGroupFiles.value?.files ?? [],
);
const groupFileUsage = computed<number>(
  () => activeGroupFiles.value?.usageBytes ?? 0,
);
const groupFileQuota = computed<number>(
  () => activeGroupFiles.value?.quotaBytes ?? 0,
);
const groupFileOverQuota = computed<boolean>(
  () =>
    groupFileQuota.value > 0 && groupFileUsage.value >= groupFileQuota.value,
);
const groupFileUsagePercent = computed<number>(() => {
  const quota = groupFileQuota.value;
  if (quota <= 0) return 0;
  return Math.min(100, Math.round((groupFileUsage.value / quota) * 100));
});

/** Friends offered in the create dialog (filtered by the search box). */
const groupCandidateFriends = computed(() => {
  const query = groupMemberFilter.value.trim().toLowerCase();
  if (query.length === 0) return friends.friends;
  return friends.friends.filter((f) => {
    const name = (f.display_name ?? f.username).toLowerCase();
    return name.includes(query) || f.username.toLowerCase().includes(query);
  });
});

const groupNameValid = computed(() => {
  const name = groupNameInput.value.trim();
  return name.length > 0 && name.length <= GROUP_NAME_MAX;
});

function openGroupDialog(): void {
  groupDialogOpen.value = true;
  groupCreateError.value = "";
  groupNameInput.value = "";
  groupMemberFilter.value = "";
  groupSelected.value = [];
  if (!friends.loaded) void friends.loadAll();
}

function closeGroupDialog(): void {
  groupDialogOpen.value = false;
}

function toggleGroupMember(username: string): void {
  groupSelected.value = groupSelected.value.includes(username)
    ? groupSelected.value.filter((u) => u !== username)
    : [...groupSelected.value, username];
}

function isGroupMemberSelected(username: string): boolean {
  return groupSelected.value.includes(username);
}

async function submitCreateGroup(): Promise<void> {
  const name = groupNameInput.value.trim();
  if (!groupNameValid.value || creatingGroup.value) return;
  creatingGroup.value = true;
  groupCreateError.value = "";
  try {
    await ws.createGroup(name, groupSelected.value);
    closeGroupDialog();
  } catch (error) {
    groupCreateError.value = groupApiErrorMessage(error, (key) => t(key));
  } finally {
    creatingGroup.value = false;
  }
}

function openGroupPanel(): void {
  const id = ws.activeConversationId;
  if (id === null) return;
  groupPanelOpen.value = true;
  groupActionError.value = "";
  groupInviteUsername.value = "";
  fileError.value = "";
  fileDeleteTarget.value = null;
  void groups.fetchInfo(id);
  // Files section is visible: subscribe so group.updated keeps it fresh.
  groups.hostFilesView(id);
}

function closeGroupPanel(): void {
  groupPanelOpen.value = false;
  groupConfirm.value = null;
  groupDescriptionEditing.value = false;
  titleDialogMember.value = null;
  fileDeleteTarget.value = null;
  fileError.value = "";
  fileDragActive.value = false;
  fileDragDepth = 0;
  groups.unhostFilesView();
}

function memberLabelOf(member: GroupMember): string {
  const display = member.display_name?.trim() ?? "";
  return display.length > 0 ? display : member.username;
}

function memberRoleLabel(role: GroupRole): string {
  return t(roleLabelKey(role));
}

// ---------------------------------------------------------------------
// M13b group files: upload / download / delete + drag & drop
// ---------------------------------------------------------------------

type GroupFileIconKind =
  "image" | "video" | "audio" | "pdf" | "zip" | "text" | "other";

/** Coarse MIME → icon category used by the file rows. */
function groupFileIconKind(file: GroupFile): GroupFileIconKind {
  const mime = file.mime.toLowerCase();
  if (mime.startsWith("image/")) return "image";
  if (mime.startsWith("video/")) return "video";
  if (mime.startsWith("audio/")) return "audio";
  if (mime === "application/pdf") return "pdf";
  if (
    mime === "application/zip" ||
    mime === "application/x-zip-compressed" ||
    mime.includes("compressed") ||
    mime.includes("tar")
  ) {
    return "zip";
  }
  if (mime.startsWith("text/")) return "text";
  return "other";
}

/** Locale short date for a file row (empty when the timestamp is invalid). */
function groupFileDate(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "";
  return date.toLocaleDateString();
}

/** Owner/admin or the original uploader may delete a file. */
function canDeleteGroupFile(file: GroupFile): boolean {
  if (file.uploader.user_id === myUserId.value) return true;
  const role = myGroupRole.value;
  return role === "owner" || role === "admin";
}

function groupFileErrorMessage(error: unknown): string {
  if (error instanceof GroupFileError && error.code === "too_large") {
    return t("group.filesTooLarge", {
      limit: formatBytes(GROUP_FILE_MAX_BYTES),
    });
  }
  return t("group.filesUploadFailed");
}

/** Uploads dropped/picked files sequentially, collecting per-file failures. */
async function uploadGroupFiles(files: File[]): Promise<void> {
  const id = ws.activeConversationId;
  if (id === null || files.length === 0 || fileUploading.value) return;
  fileError.value = "";
  fileUploading.value = true;
  fileUploadPercent.value = 0;
  const failures: string[] = [];
  try {
    for (const file of files) {
      fileUploadPercent.value = 0;
      try {
        await groups.uploadFile(id, file, (percent) => {
          fileUploadPercent.value = percent;
        });
      } catch (error) {
        failures.push(`${file.name}: ${groupFileErrorMessage(error)}`);
      }
    }
  } finally {
    fileUploading.value = false;
    fileUploadPercent.value = 0;
  }
  if (failures.length > 0) fileError.value = failures.join(" · ");
}

function pickGroupFile(): void {
  fileError.value = "";
  groupFileInputEl.value?.click();
}

async function onGroupFilePicked(event: Event): Promise<void> {
  const input = event.target as HTMLInputElement;
  const files = Array.from(input.files ?? []);
  // Reset so picking the same file twice still fires `change`.
  input.value = "";
  await uploadGroupFiles(files);
}

async function downloadGroupFileAction(file: GroupFile): Promise<void> {
  if (fileBusyId.value !== null) return;
  fileBusyId.value = file.file_id;
  fileError.value = "";
  try {
    const { blob, name } = await groups.downloadFile(file.file_id);
    saveBlobAs(blob, name.length > 0 ? name : file.name);
  } catch {
    fileError.value = t("group.filesDownloadFailed");
  } finally {
    fileBusyId.value = null;
  }
}

function askDeleteGroupFile(file: GroupFile): void {
  fileDeleteTarget.value = file;
}

function cancelDeleteGroupFile(): void {
  fileDeleteTarget.value = null;
}

async function confirmDeleteGroupFile(): Promise<void> {
  const target = fileDeleteTarget.value;
  const id = ws.activeConversationId;
  fileDeleteTarget.value = null;
  if (target === null || id === null) return;
  fileBusyId.value = target.file_id;
  fileError.value = "";
  try {
    await groups.deleteFile(id, target.file_id);
  } catch {
    fileError.value = t("group.filesDeleteFailed");
  } finally {
    fileBusyId.value = null;
  }
}

// Drag & drop: a depth counter keeps the highlight stable while the pointer
// crosses the nested children of the drop zone.
function onFileDragEnter(): void {
  fileDragDepth += 1;
  fileDragActive.value = true;
}

function onFileDragLeave(): void {
  fileDragDepth = Math.max(0, fileDragDepth - 1);
  if (fileDragDepth === 0) fileDragActive.value = false;
}

function onFileDragOver(): void {
  // The `.prevent` modifier blocks the browser's default "open the file".
  fileDragActive.value = true;
}

async function onFileDrop(event: DragEvent): Promise<void> {
  fileDragDepth = 0;
  fileDragActive.value = false;
  const dropped = event.dataTransfer?.files;
  if (dropped === undefined || dropped.length === 0) return;
  await uploadGroupFiles(Array.from(dropped));
}

// ---------------------------------------------------------------------
// M12b app settings: theme / language / version / sponsor
// ---------------------------------------------------------------------

const themeOptions = computed<{ value: ThemePreference; label: string }[]>(
  () => [
    { value: "light", label: t("settings.themeLight") },
    { value: "dark", label: t("settings.themeDark") },
    { value: "system", label: t("settings.themeSystem") },
  ],
);
const appVersion: string = APP_VERSION;

function openSettings(): void {
  settingsOpen.value = true;
}

function closeSettings(): void {
  settingsOpen.value = false;
}

function setTheme(value: ThemePreference): void {
  themePreference.value = value;
}

function setLocale(value: AppLocale): void {
  locale.value = value;
  persistLocale(value);
}

// ---------------------------------------------------------------------
// M12b group settings: description, avatar, member titles
// ---------------------------------------------------------------------

/** Owner/admin gate for group profile settings (description + avatar). */
const canEditGroup = computed<boolean>(() => {
  const info = activeGroupInfo.value;
  return info !== null && canInviteMembers(info.members, myUserId.value);
});

const groupDescriptionText = computed<string>(
  () => activeGroupInfo.value?.description?.trim() ?? "",
);

const groupDescriptionTooLong = computed<boolean>(
  () => groupDescriptionDraft.value.length > GROUP_DESCRIPTION_MAX,
);

const groupHasAvatar = computed<boolean>(
  () => (activeGroupInfo.value?.avatar ?? "").trim().length > 0,
);

function startDescriptionEdit(): void {
  groupDescriptionDraft.value = activeGroupInfo.value?.description ?? "";
  groupDescriptionEditing.value = true;
  groupActionError.value = "";
}

function cancelDescriptionEdit(): void {
  groupDescriptionEditing.value = false;
}

async function saveDescription(): Promise<void> {
  const id = ws.activeConversationId;
  if (
    id === null ||
    groupDescriptionSaving.value ||
    groupDescriptionTooLong.value
  ) {
    return;
  }
  groupDescriptionSaving.value = true;
  groupActionError.value = "";
  try {
    await groups.updateGroup(id, {
      description: groupDescriptionDraft.value.trim(),
    });
    groupDescriptionEditing.value = false;
  } catch (error) {
    groupActionError.value = groupApiErrorMessage(error, (key) => t(key));
  } finally {
    groupDescriptionSaving.value = false;
  }
}

function pickGroupAvatar(): void {
  groupActionError.value = "";
  groupAvatarInputEl.value?.click();
}

/** Picked group avatar → compress to a 256px JPEG data URL → PATCH avatar. */
async function onGroupAvatarFile(event: Event): Promise<void> {
  const input = event.target as HTMLInputElement;
  const file = input.files?.[0] ?? null;
  // Reset so picking the same file twice still fires `change`.
  input.value = "";
  const id = ws.activeConversationId;
  if (file === null || id === null) return;
  try {
    const dataUrl = await fileToAvatarDataUrl(file);
    if (dataUrl === null) return;
    groupActionError.value = "";
    await groups.updateGroup(id, { avatar: dataUrl });
  } catch (error) {
    groupActionError.value = groupApiErrorMessage(error, (key) => t(key));
  }
}

async function removeGroupAvatar(): Promise<void> {
  const id = ws.activeConversationId;
  if (id === null) return;
  groupActionError.value = "";
  try {
    await groups.updateGroup(id, { avatar: "" });
  } catch (error) {
    groupActionError.value = groupApiErrorMessage(error, (key) => t(key));
  }
}

interface MemberTitleBadge {
  text: string;
  kind: "custom" | "role" | "tier";
}

/**
 * Display title for a member: custom_title → role label (owner/admin) → tier
 * title. Preferring the local resolution keeps role/tier badges localized to
 * the current UI locale; the server-resolved `title` is the fallback.
 */
function memberTitleInfo(member: GroupMember): MemberTitleBadge | null {
  const custom = member.custom_title?.trim() ?? "";
  if (custom.length > 0) return { text: custom, kind: "custom" };
  if (member.role === "owner" || member.role === "admin") {
    return { text: t(roleLabelKey(member.role)), kind: "role" };
  }
  if (typeof member.group_level === "number" && member.group_level > 0) {
    return { text: t(groupTierTitleKey(member.group_level)), kind: "tier" };
  }
  const resolved = member.title?.trim() ?? "";
  return resolved.length > 0 ? { text: resolved, kind: "tier" } : null;
}

/** Color chip class for a member's title (custom violet, role indigo/amber, tier neutral). */
function memberTitleClass(member: GroupMember): string {
  const info = memberTitleInfo(member);
  if (info === null) return "";
  if (info.kind === "custom") {
    return "bg-violet-100 text-violet-700 dark:bg-violet-900/40 dark:text-violet-300";
  }
  if (info.kind === "role") {
    return member.role === "owner"
      ? "bg-indigo-100 text-indigo-700 dark:bg-indigo-900/40 dark:text-indigo-300"
      : "bg-amber-100 text-amber-700 dark:bg-amber-900/40 dark:text-amber-300";
  }
  return "bg-neutral-100 text-neutral-600 dark:bg-neutral-800 dark:text-neutral-300";
}

/** "Lv.N" label for a member's group level (null when unknown). */
function memberLevelLabel(member: GroupMember): string | null {
  if (typeof member.group_level !== "number" || member.group_level <= 0) {
    return null;
  }
  return t("group.levelLabel", { level: member.group_level });
}

function canSetTitle(member: GroupMember): boolean {
  const info = activeGroupInfo.value;
  return (
    info !== null &&
    canSetMemberTitle(info.members, myUserId.value, member.user_id)
  );
}

function openTitleDialog(member: GroupMember): void {
  titleDialogMember.value = member;
  titleDraft.value = member.custom_title?.trim() ?? "";
  groupActionError.value = "";
}

function closeTitleDialog(): void {
  titleDialogMember.value = null;
}

async function saveTitle(): Promise<void> {
  const member = titleDialogMember.value;
  const id = ws.activeConversationId;
  if (member === null || id === null || titleSaving.value) return;
  const title = titleDraft.value.trim();
  titleSaving.value = true;
  groupActionError.value = "";
  try {
    await groups.setMemberTitle(
      id,
      member.user_id,
      title.length > 0 ? title : null,
    );
    titleDialogMember.value = null;
  } catch (error) {
    groupActionError.value = groupApiErrorMessage(error, (key) => t(key));
  } finally {
    titleSaving.value = false;
  }
}

async function clearTitle(): Promise<void> {
  const member = titleDialogMember.value;
  const id = ws.activeConversationId;
  if (member === null || id === null || titleSaving.value) return;
  titleSaving.value = true;
  groupActionError.value = "";
  try {
    await groups.setMemberTitle(id, member.user_id, null);
    titleDraft.value = "";
    titleDialogMember.value = null;
  } catch (error) {
    groupActionError.value = groupApiErrorMessage(error, (key) => t(key));
  } finally {
    titleSaving.value = false;
  }
}

// Role-gated capabilities for the current member (delegated to pure helpers).
function canKick(member: GroupMember): boolean {
  const info = activeGroupInfo.value;
  return (
    info !== null && canKickMember(info.members, myUserId.value, member.user_id)
  );
}
function canChangeMemberRole(member: GroupMember): boolean {
  const info = activeGroupInfo.value;
  return (
    info !== null && canChangeRole(info.members, myUserId.value, member.user_id)
  );
}
function canTransferTo(member: GroupMember): boolean {
  const info = activeGroupInfo.value;
  return (
    info !== null &&
    canTransferOwnership(info.members, myUserId.value, member.user_id)
  );
}
const canInvite = computed<boolean>(() => {
  const info = activeGroupInfo.value;
  return info !== null && canInviteMembers(info.members, myUserId.value);
});
const canLeave = computed<boolean>(() => {
  const info = activeGroupInfo.value;
  return info !== null && canLeaveGroup(info.members, myUserId.value);
});

function askKick(member: GroupMember): void {
  groupConfirm.value = {
    type: "kick",
    userId: member.user_id,
    message: t("group.confirmKick", { name: memberLabelOf(member) }),
  };
}

function askRoleChange(member: GroupMember, role: "admin" | "member"): void {
  groupConfirm.value = {
    type: "role",
    userId: member.user_id,
    role,
    message:
      role === "admin"
        ? t("group.confirmAppoint", { name: memberLabelOf(member) })
        : t("group.confirmDemote", { name: memberLabelOf(member) }),
  };
}

function askTransfer(member: GroupMember): void {
  groupConfirm.value = {
    type: "transfer",
    userId: member.user_id,
    message: t("group.confirmTransfer", { name: memberLabelOf(member) }),
  };
}

function askLeave(): void {
  groupConfirm.value = { type: "leave", message: t("group.confirmLeave") };
}

function cancelGroupConfirm(): void {
  groupConfirm.value = null;
}

/** Runs the confirmed destructive action, then relies on refetch for state. */
async function runGroupConfirm(): Promise<void> {
  const action = groupConfirm.value;
  const id = ws.activeConversationId;
  if (action === null || id === null) return;
  groupConfirm.value = null;
  groupActionError.value = "";
  busyMemberId.value = action.userId ?? null;
  try {
    if (action.type === "kick" && action.userId !== undefined) {
      await groups.kickMember(id, action.userId);
    } else if (
      action.type === "role" &&
      action.userId !== undefined &&
      action.role !== undefined
    ) {
      await groups.changeMemberRole(id, action.userId, action.role);
    } else if (action.type === "transfer" && action.userId !== undefined) {
      await groups.transferOwnership(id, action.userId);
    } else if (action.type === "leave") {
      await groups.leave(id);
      closeGroupPanel();
    }
  } catch (error) {
    groupActionError.value = groupApiErrorMessage(error, (key) => t(key));
  } finally {
    busyMemberId.value = null;
  }
}

async function submitGroupInvite(): Promise<void> {
  const id = ws.activeConversationId;
  const username = groupInviteUsername.value.trim();
  if (id === null || username.length === 0) return;
  groupActionError.value = "";
  try {
    await groups.inviteMember(id, username);
    groupInviteUsername.value = "";
  } catch (error) {
    groupActionError.value = groupApiErrorMessage(error, (key) => t(key));
  }
}

// ---------------------------------------------------------------------
// M14 calls + screen share
// ---------------------------------------------------------------------

/** Phone button in the thread header / group drawer. */
function startCallFromThread(): void {
  const id = ws.activeConversationId;
  if (id === null) return;
  sharePickerOpen.value = false;
  calls.startCall(id);
}

function acceptCall(): void {
  void calls.acceptIncoming();
}

function rejectCall(): void {
  calls.rejectIncoming();
}

function hangupCall(): void {
  calls.hangup();
}

function toggleCallMute(): void {
  calls.toggleMute();
}

/** Pre-share it toggles the picker; while sharing it stops the share. */
function toggleSharePicker(): void {
  if (calls.sharing) {
    void calls.stopShare();
    return;
  }
  sharePickerOpen.value = !sharePickerOpen.value;
}

function confirmShare(): void {
  sharePickerOpen.value = false;
  void calls.startShare(selectedShareQuality.value);
}

function stopShare(): void {
  void calls.stopShare();
}

function selectShareResolution(label: string): void {
  shareResolutionLabel.value = label;
}

function selectShareFrameRate(fps: number): void {
  shareFrameRate.value = fps;
}

/** Live quality switch while already sharing (replaceTrack per mesh leg). */
function pickShareQuality(label: string, fps: number): void {
  shareResolutionLabel.value = label;
  shareFrameRate.value = fps;
  if (calls.sharing) void calls.changeShareQuality(shareQuality(label, fps));
}

function shareQualityLabel(label: string): string {
  return t(`call.quality${label}`);
}

function shareFpsLabel(fps: number): string {
  return t(`call.fps${fps}`);
}

interface CallParticipantView {
  username: string;
  displayName?: string;
  avatar?: string | null;
}

function participantRef(userId: string): CallParticipantView {
  if (userId === calls.myUserId()) {
    return {
      username: auth.user?.username ?? "",
      displayName: auth.user?.displayName,
      avatar: auth.user?.avatar ?? null,
    };
  }
  if (calls.peer !== null && userId === calls.peer.userId) {
    return {
      username: calls.peer.username,
      displayName: calls.peer.displayName,
      avatar: null,
    };
  }
  const participant = calls.participants.find((p) => p.user_id === userId);
  return {
    username: participant?.username ?? userId,
    displayName: participant?.display_name,
    avatar: null,
  };
}

function speakerName(userId: string): string {
  if (userId === calls.myUserId()) return t("call.you");
  const ref = participantRef(userId);
  const display = ref.displayName?.trim() ?? "";
  return display.length > 0 ? display : ref.username;
}

function speakingOf(userId: string): boolean {
  return calls.speaking[userId] === true;
}
</script>

<template>
  <AppShell>
    <template #nav>
      <div
        class="flex w-full items-center gap-2 lg:flex-col lg:gap-2 lg:px-1 lg:py-1"
        data-testid="chat-nav"
      >
        <span class="text-sm font-bold tracking-tight lg:text-[11px]">{{
          t("app.name")
        }}</span>
        <button
          type="button"
          class="ml-auto flex items-center gap-1.5 rounded-full p-0.5 hover:bg-neutral-100 dark:hover:bg-neutral-800 lg:ml-0 lg:flex-col lg:gap-0.5 lg:p-1"
          data-testid="nav-self"
          :title="t('nav.profile')"
          @click="openSelfProfile()"
        >
          <Avatar
            :username="auth.user?.username ?? ''"
            :display-name="auth.user?.displayName"
            :avatar="auth.user?.avatar"
            :size="28"
          />
          <span
            class="max-w-[6rem] truncate text-[10px] text-neutral-500 dark:text-neutral-400"
            data-testid="nav-username"
          >
            {{ auth.user?.username ?? t("nav.anonymous") }}
          </span>
        </button>
        <ThemeToggle />
        <LanguageToggle />
        <router-link
          to="/contacts"
          data-testid="nav-contacts"
          class="relative inline-flex h-9 items-center justify-center gap-1 whitespace-nowrap rounded-lg px-2 text-xs font-medium text-neutral-600 hover:bg-neutral-100 hover:text-neutral-900 lg:w-full dark:text-neutral-300 dark:hover:bg-neutral-800 dark:hover:text-white"
          :class="{
            'bg-neutral-100 text-neutral-900 dark:bg-neutral-800 dark:text-white':
              route.name === 'contacts',
          }"
        >
          {{ t("nav.contacts") }}
          <span
            v-if="friends.pendingCount + groups.pendingInviteCount > 0"
            data-testid="nav-contacts-badge"
            class="flex h-4 min-w-4 items-center justify-center rounded-full bg-indigo-600 px-1 text-[10px] font-semibold text-white"
          >
            {{ friends.pendingCount + groups.pendingInviteCount }}
          </span>
        </router-link>
        <button
          type="button"
          class="inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-lg text-neutral-600 hover:bg-neutral-100 hover:text-neutral-900 lg:mt-auto lg:w-full dark:text-neutral-300 dark:hover:bg-neutral-800 dark:hover:text-white"
          data-testid="settings-button"
          :title="t('settings.button')"
          :aria-label="t('settings.button')"
          @click="openSettings()"
        >
          <svg
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.8"
            stroke-linecap="round"
            stroke-linejoin="round"
            class="h-5 w-5"
            aria-hidden="true"
          >
            <circle cx="12" cy="12" r="3" />
            <path
              d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1Z"
            />
          </svg>
        </button>
        <button
          type="button"
          data-testid="logout-button"
          class="inline-flex h-9 items-center justify-center whitespace-nowrap rounded-lg px-2 text-xs font-medium text-neutral-600 hover:bg-neutral-100 hover:text-neutral-900 lg:w-full dark:text-neutral-300 dark:hover:bg-neutral-800 dark:hover:text-white"
          @click="logout()"
        >
          {{ t("nav.logout") }}
        </button>
      </div>
    </template>

    <template #sessions>
      <div class="flex h-full flex-col p-3">
        <h2
          class="px-1 pb-2 text-sm font-semibold text-neutral-500 dark:text-neutral-400"
          data-testid="sessions-title"
        >
          {{ t("chat.sessionsTitle") }}
        </h2>

        <!-- New conversation row -->
        <form
          class="flex items-center gap-2 pb-2"
          @submit.prevent="submitNewConversation()"
        >
          <input
            v-model="newPeerUsername"
            type="text"
            data-testid="new-conversation-input"
            :placeholder="t('chat.newConversationPlaceholder')"
            class="min-w-0 flex-1 rounded-lg border border-neutral-300 bg-transparent px-2 py-1.5 text-sm outline-none focus:border-indigo-500 dark:border-neutral-700"
          />
          <!-- M3: conversation kind segmented control (普通 / 密聊) -->
          <div
            class="flex shrink-0 overflow-hidden rounded-lg border border-neutral-300 text-xs dark:border-neutral-700"
            data-testid="kind-toggle"
          >
            <button
              type="button"
              data-testid="kind-direct"
              class="px-2 py-1.5 font-medium transition-colors"
              :class="
                newConversationKind === 'direct'
                  ? 'bg-indigo-600 text-white'
                  : 'text-neutral-500 hover:bg-neutral-100 dark:text-neutral-400 dark:hover:bg-neutral-800'
              "
              @click="newConversationKind = 'direct'"
            >
              {{ t("chat.kindDirect") }}
            </button>
            <button
              type="button"
              data-testid="kind-secret"
              class="border-l border-neutral-300 px-2 py-1.5 font-medium transition-colors dark:border-neutral-700"
              :class="
                newConversationKind === 'secret'
                  ? 'bg-emerald-600 text-white'
                  : 'text-neutral-500 hover:bg-neutral-100 dark:text-neutral-400 dark:hover:bg-neutral-800'
              "
              @click="newConversationKind = 'secret'"
            >
              {{ t("chat.kindSecret") }}
            </button>
          </div>
          <button
            type="submit"
            data-testid="new-conversation-submit"
            :disabled="
              creatingConversation || newPeerUsername.trim().length === 0
            "
            class="shrink-0 rounded-lg bg-indigo-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-50"
          >
            {{
              creatingConversation
                ? t("chat.newConversationCreating")
                : t("chat.newConversationSubmit")
            }}
          </button>
        </form>
        <p
          v-if="conversationError.length > 0"
          class="px-1 pb-2 text-xs text-red-500"
          data-testid="new-conversation-error"
        >
          {{ conversationError }}
        </p>

        <!-- M11: group creation affordance -->
        <button
          type="button"
          data-testid="new-group-button"
          class="mb-2 w-full rounded-lg border border-dashed border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:border-indigo-400 hover:text-indigo-600 dark:border-neutral-700 dark:text-neutral-300 dark:hover:border-indigo-500 dark:hover:text-indigo-400"
          @click="openGroupDialog()"
        >
          {{ t("group.newGroup") }}
        </button>

        <!-- Conversation list; the grey pill behind the active row glides via
             syncSessionIndicator() (screen-reader hidden, not a real row) -->
        <ul
          v-if="ws.sortedConversations.length > 0"
          class="relative space-y-1"
          data-testid="session-list"
        >
          <li
            aria-hidden="true"
            data-testid="session-indicator"
            class="pointer-events-none absolute inset-x-0 top-0 z-0 rounded-lg bg-neutral-100 will-change-transform dark:bg-neutral-800"
            :class="indicatorTransitionClass"
            :style="indicatorStyle"
          ></li>
          <li
            v-for="conversation in ws.sortedConversations"
            :key="conversation.conversationId"
            data-testid="session-item"
            :data-conversation-id="conversation.conversationId"
            class="relative z-10 flex cursor-pointer items-center gap-3 rounded-lg p-2 hover:bg-neutral-100 dark:hover:bg-neutral-800"
            @click="openConversation(conversation.conversationId)"
          >
            <Avatar
              class="shrink-0"
              :group="conversation.kind === 'group'"
              :username="
                conversation.kind === 'group'
                  ? (conversation.name ?? '')
                  : conversation.peerUsername
              "
              :display-name="
                conversation.kind === 'group'
                  ? undefined
                  : conversation.peerDisplayName
              "
              :avatar="
                conversation.kind === 'group' ? null : conversation.peerAvatar
              "
              :size="36"
              data-testid="session-avatar"
              @click="openSessionAvatar(conversation)"
            />
            <span class="min-w-0 flex-1">
              <span class="flex items-baseline justify-between gap-2">
                <span
                  class="flex min-w-0 items-center gap-1 text-sm font-medium"
                >
                  <svg
                    v-if="conversation.kind === 'secret'"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="2"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                    class="h-3 w-3 shrink-0 text-emerald-600 dark:text-emerald-400"
                    data-testid="session-lock"
                    aria-hidden="true"
                  >
                    <rect x="5" y="11" width="14" height="9" rx="2" />
                    <path d="M8 11V7a4 4 0 0 1 8 0v4" />
                  </svg>
                  <span class="min-w-0 truncate" data-testid="session-name">{{
                    peerLabel(conversation)
                  }}</span>
                </span>
                <span
                  class="shrink-0 text-[11px] text-neutral-400 dark:text-neutral-500"
                  data-testid="session-time"
                  >{{ relativeTime(conversation.lastActivityAt) }}</span
                >
              </span>
              <span class="flex items-center justify-between gap-2">
                <span
                  class="min-w-0 truncate text-xs text-neutral-500 dark:text-neutral-400"
                  data-testid="session-preview"
                  >{{ conversationPreview(conversation) }}</span
                >
                <span
                  v-if="conversation.unread > 0"
                  class="flex h-5 min-w-5 shrink-0 items-center justify-center rounded-full bg-indigo-600 px-1.5 text-[11px] font-semibold text-white"
                  data-testid="unread-badge"
                  >{{ conversation.unread }}</span
                >
              </span>
            </span>
          </li>
        </ul>
        <p
          v-else
          class="px-1 pt-2 text-xs text-neutral-400 dark:text-neutral-500"
          data-testid="sessions-empty"
        >
          {{ t("chat.sessionsEmpty") }}
        </p>
      </div>
    </template>

    <template #main>
      <!-- Empty state: no conversation selected -->
      <div
        v-if="ws.activeConversation === null"
        class="flex h-full items-center justify-center"
      >
        <p
          class="text-sm text-neutral-400 dark:text-neutral-500"
          data-testid="chat-empty-state"
        >
          {{ t("chat.emptyState") }}
        </p>
      </div>

      <div v-else class="flex h-full min-h-0 flex-col">
        <!-- Disconnected banner -->
        <div
          v-if="!ws.isConnected"
          class="flex shrink-0 items-center justify-center bg-amber-100 py-1.5 text-xs font-medium text-amber-800 dark:bg-amber-900/40 dark:text-amber-200"
          data-testid="ws-banner"
        >
          {{ t("chat.reconnectingBanner") }}
        </div>
        <button
          v-if="ws.lastError !== null"
          type="button"
          class="shrink-0 bg-red-100 py-1.5 text-xs font-medium text-red-700 dark:bg-red-900/40 dark:text-red-200"
          data-testid="ws-error"
          @click="ws.lastError = null"
        >
          {{ t("chat.wsError") }}
        </button>

        <!-- M3 secret chat banner: device-bound, never synced elsewhere -->
        <div
          v-if="ws.activeConversation?.kind === 'secret'"
          class="flex shrink-0 items-center justify-center gap-1.5 bg-emerald-100 py-1.5 text-xs font-medium text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-200"
          data-testid="secret-banner"
        >
          <svg
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            class="h-3 w-3 shrink-0"
            aria-hidden="true"
          >
            <rect x="5" y="11" width="14" height="9" rx="2" />
            <path d="M8 11V7a4 4 0 0 1 8 0v4" />
          </svg>
          {{ t("chat.secretBanner") }}
        </div>

        <!-- Thread header -->
        <header
          class="flex shrink-0 items-center gap-2 border-b border-neutral-200 px-4 py-2 dark:border-neutral-800"
        >
          <Avatar
            class="shrink-0"
            :group="ws.activeConversation.kind === 'group'"
            :username="
              ws.activeConversation.kind === 'group'
                ? (ws.activeConversation.name ?? '')
                : ws.activeConversation.peerUsername
            "
            :display-name="
              ws.activeConversation.kind === 'group'
                ? undefined
                : ws.activeConversation.peerDisplayName
            "
            :avatar="
              ws.activeConversation.kind === 'group'
                ? null
                : ws.activeConversation.peerAvatar
            "
            :size="32"
            data-testid="thread-avatar"
            @click="openSessionAvatar(ws.activeConversation)"
          />
          <span
            class="min-w-0 truncate text-sm font-semibold"
            data-testid="thread-title"
          >
            {{ peerLabel(ws.activeConversation) }}
          </span>
          <span
            v-if="
              ws.activeConversation.kind === 'group' && activeGroupInfo !== null
            "
            class="shrink-0 text-xs text-neutral-400 dark:text-neutral-500"
            data-testid="thread-member-count"
          >
            {{ t("group.memberCount", { n: activeGroupInfo.member_count }) }}
          </span>
          <svg
            v-if="ws.activeConversation.kind === 'secret'"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            class="h-3.5 w-3.5 shrink-0 text-emerald-600 dark:text-emerald-400"
            data-testid="thread-lock"
            aria-hidden="true"
          >
            <rect x="5" y="11" width="14" height="9" rx="2" />
            <path d="M8 11V7a4 4 0 0 1 8 0v4" />
          </svg>
          <button
            v-if="callsEnabled"
            type="button"
            class="ml-auto shrink-0 rounded-lg border border-neutral-300 p-1.5 text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
            data-testid="call-start"
            :title="
              ws.activeConversation.kind === 'group'
                ? t('call.groupCall')
                : t('call.start')
            "
            :aria-label="t('call.start')"
            @click="startCallFromThread()"
          >
            <svg
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              stroke-width="2"
              stroke-linecap="round"
              stroke-linejoin="round"
              class="h-4 w-4"
              aria-hidden="true"
            >
              <path
                d="M22 16.92v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.5 19.5 0 0 1-6-6 19.79 19.79 0 0 1-3.07-8.67A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72c.13.96.36 1.9.7 2.81a2 2 0 0 1-.45 2.11L8.09 9.91a16 16 0 0 0 6 6l1.27-1.27a2 2 0 0 1 2.11-.45c.91.34 1.85.57 2.81.7A2 2 0 0 1 22 16.92z"
              />
            </svg>
          </button>
          <button
            v-if="ws.activeConversation.kind === 'group'"
            type="button"
            class="ml-1 shrink-0 rounded-lg border border-neutral-300 px-2 py-1 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
            data-testid="group-info-button"
            @click="openGroupPanel()"
          >
            {{ t("group.infoButton") }}
          </button>
        </header>

        <!-- M14 active call panel: participants, controls, screen stage -->
        <div
          v-if="calls.isInCall && calls.status !== 'incoming'"
          class="shrink-0 border-b border-neutral-200 px-3 py-2 dark:border-neutral-800"
          data-testid="call-panel"
        >
          <div class="flex items-center gap-2">
            <span
              class="text-xs font-semibold text-neutral-700 dark:text-neutral-200"
              data-testid="call-status"
            >
              {{ callStatusLabel }}
            </span>
            <span
              class="text-xs tabular-nums text-neutral-400 dark:text-neutral-500"
              data-testid="call-duration"
            >
              {{ callDuration }}
            </span>
            <div class="ml-auto flex items-center gap-1">
              <button
                type="button"
                class="rounded-lg border border-neutral-300 p-1.5 hover:bg-neutral-100 dark:border-neutral-700 dark:hover:bg-neutral-800"
                :class="
                  calls.muted
                    ? 'text-red-500'
                    : 'text-neutral-600 dark:text-neutral-300'
                "
                data-testid="call-mute"
                :title="calls.muted ? t('call.unmute') : t('call.mute')"
                :aria-label="calls.muted ? t('call.unmute') : t('call.mute')"
                @click="toggleCallMute()"
              >
                <svg
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="2"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                  class="h-4 w-4"
                  aria-hidden="true"
                >
                  <rect x="9" y="2" width="6" height="12" rx="3" />
                  <path d="M5 10a7 7 0 0 0 14 0" />
                  <path d="M12 17v5" />
                </svg>
              </button>
              <button
                type="button"
                class="rounded-lg border border-neutral-300 p-1.5 hover:bg-neutral-100 dark:border-neutral-700 dark:hover:bg-neutral-800"
                :class="
                  calls.sharing
                    ? 'text-indigo-600 dark:text-indigo-400'
                    : 'text-neutral-600 dark:text-neutral-300'
                "
                data-testid="call-share"
                :title="calls.sharing ? t('call.stopShare') : t('call.share')"
                :aria-label="
                  calls.sharing ? t('call.stopShare') : t('call.share')
                "
                @click="toggleSharePicker()"
              >
                <svg
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="2"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                  class="h-4 w-4"
                  aria-hidden="true"
                >
                  <rect x="2" y="3" width="20" height="14" rx="2" />
                  <path d="M8 21h8M12 17v4" />
                </svg>
              </button>
              <button
                type="button"
                class="rounded-lg bg-red-600 p-1.5 text-white hover:bg-red-500"
                data-testid="call-hangup"
                :title="t('call.hangup')"
                :aria-label="t('call.hangup')"
                @click="hangupCall()"
              >
                <svg
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="2"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                  class="h-4 w-4"
                  aria-hidden="true"
                >
                  <path
                    d="M22 16.92v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.5 19.5 0 0 1-6-6 19.79 19.79 0 0 1-3.07-8.67A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72c.13.96.36 1.9.7 2.81a2 2 0 0 1-.45 2.11L8.09 9.91a16 16 0 0 0 6 6l1.27-1.27a2 2 0 0 1 2.11-.45c.91.34 1.85.57 2.81.7A2 2 0 0 1 22 16.92z"
                    transform="rotate(135 12 12)"
                  />
                </svg>
              </button>
            </div>
          </div>

          <!-- Participant avatars with speaking rings -->
          <div
            class="mt-2 flex flex-wrap items-center gap-3"
            data-testid="call-participants"
          >
            <div
              v-for="uid in callParticipantIds"
              :key="uid"
              class="flex flex-col items-center gap-0.5"
              data-testid="call-participant"
            >
              <span
                class="rounded-full transition-shadow"
                :class="
                  speakingOf(uid)
                    ? 'ring-2 ring-emerald-400 ring-offset-1 dark:ring-offset-neutral-950'
                    : ''
                "
              >
                <Avatar
                  :username="participantRef(uid).username"
                  :display-name="participantRef(uid).displayName"
                  :avatar="participantRef(uid).avatar"
                  :size="40"
                />
              </span>
              <span
                class="max-w-[64px] truncate text-[10px] text-neutral-500 dark:text-neutral-400"
              >
                {{ speakerName(uid) }}
              </span>
            </div>
          </div>

          <!-- Screen stage: remote sharer large, own capture preview -->
          <div
            v-if="calls.hasVideo"
            class="relative mt-2 overflow-hidden rounded-xl bg-black/90"
            data-testid="screen-stage"
          >
            <video
              v-if="stageIsRemote"
              ref="remoteVideoEl"
              autoplay
              playsinline
              class="max-h-64 w-full object-contain"
              data-testid="screen-remote-video"
            ></video>
            <video
              v-else
              ref="selfVideoEl"
              autoplay
              playsinline
              muted
              class="max-h-64 w-full object-contain"
              data-testid="screen-self-video"
            ></video>
            <span
              v-if="calls.remoteVideoUsers.length > 1"
              class="absolute left-2 top-2 rounded bg-black/60 px-2 py-0.5 text-[10px] text-white"
              data-testid="screen-multiple"
            >
              {{ t("call.multipleSharers") }}
            </span>
            <video
              v-if="stageIsRemote && calls.sharing"
              ref="selfThumbEl"
              autoplay
              playsinline
              muted
              class="mt-1 max-h-24 w-full object-contain opacity-80"
              data-testid="screen-self-thumb"
            ></video>
          </div>

          <!-- Share quality picker (pre-share) / live switcher (while sharing) -->
          <div
            v-if="sharePickerOpen && !calls.sharing"
            class="mt-2 rounded-xl border border-neutral-200 p-2 dark:border-neutral-800"
            data-testid="share-quality"
          >
            <p
              class="text-[11px] font-medium text-neutral-500 dark:text-neutral-400"
            >
              {{ t("call.shareResolution") }}
            </p>
            <div class="mt-1 flex flex-wrap gap-1">
              <button
                v-for="res in SHARE_RESOLUTIONS"
                :key="res.label"
                type="button"
                class="rounded-lg border px-2 py-1 text-[11px] font-medium"
                :class="
                  shareResolutionLabel === res.label
                    ? 'border-indigo-500 bg-indigo-50 text-indigo-700 dark:bg-indigo-950 dark:text-indigo-200'
                    : 'border-neutral-300 text-neutral-600 dark:border-neutral-700 dark:text-neutral-300'
                "
                data-testid="share-resolution"
                :data-value="res.label"
                @click="selectShareResolution(res.label)"
              >
                {{ shareQualityLabel(res.label) }}
              </button>
            </div>
            <p
              class="mt-2 text-[11px] font-medium text-neutral-500 dark:text-neutral-400"
            >
              {{ t("call.shareFrameRate") }}
            </p>
            <div class="mt-1 flex flex-wrap gap-1">
              <button
                v-for="fps in SHARE_FRAMERATES"
                :key="fps"
                type="button"
                class="rounded-lg border px-2 py-1 text-[11px] font-medium"
                :class="
                  shareFrameRate === fps
                    ? 'border-indigo-500 bg-indigo-50 text-indigo-700 dark:bg-indigo-950 dark:text-indigo-200'
                    : 'border-neutral-300 text-neutral-600 dark:border-neutral-700 dark:text-neutral-300'
                "
                data-testid="share-fps"
                :data-value="fps"
                @click="selectShareFrameRate(fps)"
              >
                {{ shareFpsLabel(fps) }}
              </button>
            </div>
            <div class="mt-2 flex justify-end gap-2">
              <button
                type="button"
                class="rounded-lg border border-neutral-300 px-2 py-1 text-[11px] font-medium text-neutral-600 dark:border-neutral-700 dark:text-neutral-300"
                data-testid="share-cancel"
                @click="sharePickerOpen = false"
              >
                {{ t("call.shareCancel") }}
              </button>
              <button
                type="button"
                class="rounded-lg bg-indigo-600 px-3 py-1 text-[11px] font-medium text-white hover:bg-indigo-500"
                data-testid="share-confirm"
                @click="confirmShare()"
              >
                {{ t("call.shareStart") }}
              </button>
            </div>
          </div>

          <!-- While sharing: compact live quality switcher + stop -->
          <div
            v-else-if="calls.sharing"
            class="mt-2 flex flex-wrap items-center gap-1 rounded-xl border border-indigo-200 p-2 dark:border-indigo-900"
            data-testid="share-quality"
          >
            <button
              v-for="res in SHARE_RESOLUTIONS"
              :key="res.label"
              type="button"
              class="rounded-lg border px-2 py-1 text-[11px] font-medium"
              :class="
                calls.shareQuality.label === res.label
                  ? 'border-indigo-500 bg-indigo-50 text-indigo-700 dark:bg-indigo-950 dark:text-indigo-200'
                  : 'border-neutral-300 text-neutral-600 dark:border-neutral-700 dark:text-neutral-300'
              "
              data-testid="share-live-resolution"
              :data-value="res.label"
              @click="pickShareQuality(res.label, calls.shareQuality.frameRate)"
            >
              {{ shareQualityLabel(res.label) }}
            </button>
            <button
              v-for="fps in SHARE_FRAMERATES"
              :key="fps"
              type="button"
              class="rounded-lg border px-2 py-1 text-[11px] font-medium"
              :class="
                calls.shareQuality.frameRate === fps
                  ? 'border-indigo-500 bg-indigo-50 text-indigo-700 dark:bg-indigo-950 dark:text-indigo-200'
                  : 'border-neutral-300 text-neutral-600 dark:border-neutral-700 dark:text-neutral-300'
              "
              data-testid="share-live-fps"
              :data-value="fps"
              @click="pickShareQuality(calls.shareQuality.label, fps)"
            >
              {{ shareFpsLabel(fps) }}
            </button>
            <button
              type="button"
              class="ml-auto rounded-lg bg-red-600 px-2 py-1 text-[11px] font-medium text-white hover:bg-red-500"
              data-testid="share-stop"
              @click="stopShare()"
            >
              {{ t("call.stopShare") }}
            </button>
          </div>
        </div>

        <!-- M3 safety code row: compare with the peer out-of-band -->
        <div
          v-if="ws.activeConversation?.kind === 'secret'"
          class="flex shrink-0 flex-wrap items-center gap-x-2 gap-y-0.5 border-b border-neutral-200 px-4 py-1.5 text-[11px] dark:border-neutral-800"
          data-testid="sas-row"
        >
          <span class="font-medium text-neutral-500 dark:text-neutral-400">
            {{ t("chat.sasLabel") }}
          </span>
          <code
            class="font-mono text-xs font-semibold tracking-widest text-emerald-700 dark:text-emerald-300"
            data-testid="sas-code"
          >
            {{ sasCode ?? t("chat.sasPending") }}
          </code>
          <span class="text-neutral-400 dark:text-neutral-500">
            {{ t("chat.sasHint") }}
          </span>
        </div>

        <!-- Message list -->
        <div
          class="min-h-0 flex-1 overflow-y-auto px-4 py-3"
          data-testid="message-list"
        >
          <p
            v-if="dayGroups.length === 0"
            class="pt-6 text-center text-xs text-neutral-400 dark:text-neutral-500"
            data-testid="thread-empty"
          >
            {{ t("chat.emptyState") }}
          </p>
          <template v-for="group in dayGroups" :key="group.key">
            <div class="my-3 flex justify-center">
              <span
                class="rounded-full bg-neutral-100 px-3 py-0.5 text-[11px] text-neutral-500 dark:bg-neutral-800 dark:text-neutral-400"
                data-testid="date-separator"
              >
                {{ group.label }}
              </span>
            </div>
            <div
              v-for="message in group.messages"
              :key="message.messageId ?? message.clientMsgId"
              class="group mb-2 flex"
              :class="message.mine ? 'justify-end' : 'justify-start'"
              data-testid="message-item"
            >
              <div class="max-w-[75%]">
                <div
                  v-if="!message.mine"
                  class="mb-0.5 flex items-center gap-1 text-[11px] text-neutral-400 dark:text-neutral-500"
                  data-testid="message-sender"
                >
                  <!-- Group bubbles attribute the actual sender (avatar + name). -->
                  <Avatar
                    v-if="ws.activeConversation?.kind === 'group'"
                    class="shrink-0"
                    :username="message.senderId"
                    :display-name="senderLabel(message)"
                    :avatar="groupAvatarOf(message.senderId)"
                    :size="16"
                    data-testid="message-sender-avatar"
                  />
                  <span>{{ senderLabel(message) }}</span>
                </div>
                <!-- M3 undecryptable: localized placeholder replaces content -->
                <div
                  v-if="message.undecryptable"
                  class="inline-block rounded-2xl bg-slate-100 px-3 py-1.5 text-sm italic leading-relaxed text-neutral-400 break-words dark:bg-slate-800 dark:text-neutral-500"
                  data-testid="undecryptable-placeholder"
                >
                  {{ t("chat.undecryptablePlaceholder") }}
                </div>
                <!-- Recall tombstone: localized placeholder replaces content -->
                <div
                  v-else-if="message.recalled"
                  class="inline-block rounded-2xl bg-slate-100 px-3 py-1.5 text-sm italic leading-relaxed text-neutral-400 break-words dark:bg-slate-800 dark:text-neutral-500"
                  data-testid="recalled-placeholder"
                >
                  {{ t("chat.recalledPlaceholder") }}
                </div>
                <!-- M8 media: rounded image (tap → lightbox) or inline player.
                     width/height attrs reserve layout space when known. -->
                <div
                  v-else-if="message.media"
                  class="inline-block"
                  data-testid="media-bubble"
                >
                  <span
                    v-if="message.forwardedFromUsername"
                    class="mb-0.5 block text-[11px] text-neutral-400 dark:text-neutral-500"
                    data-testid="forward-badge"
                    >{{
                      t("chat.forwardedFrom", {
                        username: message.forwardedFromUsername,
                      })
                    }}</span
                  >
                  <img
                    v-if="message.media.kind === 'image'"
                    :src="mediaUrl(message.media)"
                    :width="message.media.width"
                    :height="message.media.height"
                    alt=""
                    class="h-auto max-w-[280px] cursor-zoom-in rounded-2xl"
                    data-testid="media-image"
                    @click="openLightbox(message.media)"
                  />
                  <!-- M9 voice: custom minimal player (no native chrome). -->
                  <div
                    v-else-if="message.media.kind === 'audio'"
                    class="flex min-w-[180px] max-w-[260px] flex-col rounded-2xl px-3 py-2"
                    :class="
                      message.mine
                        ? 'bg-indigo-600 text-white'
                        : 'bg-slate-100 text-neutral-900 dark:bg-slate-800 dark:text-neutral-100'
                    "
                    data-testid="media-audio"
                  >
                    <!-- M11: show the uploaded file name (generated voice-* hidden). -->
                    <span
                      v-if="audioFileName(message.media) !== null"
                      class="mb-0.5 block max-w-full truncate text-[10px] opacity-80"
                      data-testid="audio-filename"
                      >{{ audioFileName(message.media) }}</span
                    >
                    <div class="flex items-center gap-2">
                      <button
                        type="button"
                        class="shrink-0 rounded-full p-1 text-sm leading-none hover:bg-black/10 dark:hover:bg-white/10"
                        data-testid="audio-play"
                        :aria-label="
                          playingMediaId === message.media.mediaId
                            ? t('chat.voicePause')
                            : t('chat.voicePlay')
                        "
                        @click="toggleAudio(message.media)"
                      >
                        {{
                          playingMediaId === message.media.mediaId ? "❚❚" : "▶"
                        }}
                      </button>
                      <span
                        class="h-1 flex-1 overflow-hidden rounded bg-black/20 dark:bg-white/20"
                      >
                        <span
                          class="block h-1 rounded bg-current transition-[width] duration-150"
                          :style="{
                            width:
                              (playingMediaId === message.media.mediaId
                                ? audioProgress
                                : 0) *
                                100 +
                              '%',
                          }"
                          data-testid="audio-progress"
                        ></span>
                      </span>
                      <span class="shrink-0 text-[11px] tabular-nums">
                        {{ formatDuration(message.media.durationMs) }}
                      </span>
                    </div>
                  </div>
                  <template v-else>
                    <video
                      :src="mediaUrl(message.media)"
                      :width="message.media.width"
                      :height="message.media.height"
                      controls
                      preload="metadata"
                      class="h-auto max-w-[320px] rounded-2xl"
                      data-testid="media-video"
                    ></video>
                    <span
                      class="mt-0.5 block text-[11px] text-neutral-400 dark:text-neutral-500"
                      data-testid="media-size"
                      >{{ formatBytes(message.media.bytes) }}</span
                    >
                  </template>
                </div>
                <div
                  v-else
                  class="inline-block rounded-2xl px-3 py-1.5 text-sm leading-relaxed break-words"
                  :class="
                    message.mine
                      ? 'bg-indigo-600 text-white'
                      : 'bg-slate-100 text-neutral-900 dark:bg-slate-800 dark:text-neutral-100'
                  "
                  data-testid="message-bubble"
                >
                  <!-- Forward attribution (original author), above content -->
                  <span
                    v-if="message.forwardedFromUsername"
                    class="mb-1 block border-l-2 border-current/40 pl-2 text-xs font-medium opacity-80"
                    data-testid="forward-badge"
                    >{{
                      t("chat.forwardedFrom", {
                        username: message.forwardedFromUsername,
                      })
                    }}</span
                  >
                  <!-- Reply quote rendered inline from server-resolved metadata -->
                  <span
                    v-if="message.replyToBodyPreview"
                    class="mb-1 block rounded border-l-2 px-2 py-0.5 text-xs opacity-80"
                    :class="
                      message.mine
                        ? 'border-white/70 bg-white/10'
                        : 'border-indigo-400 bg-black/5 dark:bg-white/5'
                    "
                    data-testid="reply-quote"
                  >
                    {{ message.replyToBodyPreview }}
                  </span>
                  {{ message.body }}
                </div>
                <!-- Hover/tap action row: reply / forward / recall.
                     Tombstones and undecryptable placeholders offer no
                     actions — there is nothing left to act on. -->
                <div
                  v-if="!message.recalled && !message.undecryptable"
                  class="mt-0.5 flex items-center gap-2 text-[11px] opacity-0 transition-opacity group-hover:opacity-100 focus-within:opacity-100"
                  :class="message.mine ? 'justify-end' : 'justify-start'"
                  data-testid="message-actions"
                >
                  <button
                    type="button"
                    class="text-neutral-500 hover:text-indigo-600 hover:underline dark:text-neutral-400"
                    data-testid="action-reply"
                    @click="replyToMessage(message)"
                  >
                    {{ t("chat.actionReply") }}
                  </button>
                  <button
                    type="button"
                    class="text-neutral-500 hover:text-indigo-600 hover:underline dark:text-neutral-400"
                    data-testid="action-forward"
                    @click="openForwardPicker(message)"
                  >
                    {{ t("chat.actionForward") }}
                  </button>
                  <button
                    v-if="canRecall(message)"
                    type="button"
                    class="text-neutral-500 hover:text-red-500 hover:underline dark:text-neutral-400"
                    data-testid="action-recall"
                    @click="recallMessage(message)"
                  >
                    {{ t("chat.actionRecall") }}
                  </button>
                </div>
                <div
                  class="mt-0.5 flex items-center gap-1.5 text-[11px]"
                  :class="message.mine ? 'justify-end' : 'justify-start'"
                >
                  <span
                    class="text-neutral-400 dark:text-neutral-500"
                    data-testid="message-time"
                  >
                    {{ clockTime(message.sentAt) }}
                  </span>
                  <span
                    v-if="message.mine && message.status === 'sending'"
                    class="animate-pulse text-indigo-500"
                    data-testid="message-status"
                  >
                    ✓ {{ t("chat.statusSending") }}
                  </span>
                  <span
                    v-else-if="message.mine && message.status === 'delivered'"
                    class="text-emerald-600 dark:text-emerald-400"
                    data-testid="message-status"
                  >
                    ✓✓ {{ t("chat.statusDelivered") }}
                  </span>
                  <span
                    v-else-if="message.mine && message.status === 'read'"
                    class="font-medium text-blue-500 dark:text-blue-400"
                    data-testid="message-status"
                  >
                    ✓✓ {{ t("chat.statusRead") }}
                  </span>
                  <button
                    v-else-if="message.mine && message.status === 'failed'"
                    type="button"
                    class="font-medium text-red-500 hover:underline"
                    data-testid="retry-button"
                    @click="retryMessage(message.clientMsgId)"
                  >
                    ⚠ {{ t("chat.statusFailed") }}
                  </button>
                </div>
              </div>
            </div>
          </template>
          <div ref="messagesEndRef"></div>
        </div>

        <!-- Peer typing indicator -->
        <div
          v-if="ws.activeConversation?.peerTypingUntil !== null"
          class="shrink-0 px-4 pb-1 text-xs text-neutral-500 dark:text-neutral-400"
          data-testid="typing-indicator"
        >
          {{ t("chat.typingIndicator") }}
        </div>

        <!-- Composer -->
        <footer
          class="relative flex shrink-0 flex-col border-t border-neutral-200 dark:border-neutral-800"
        >
          <!-- Reply context strip above the input -->
          <div
            v-if="ws.replyContext !== null"
            class="flex items-center gap-2 border-b border-neutral-200 bg-neutral-50 px-3 py-1.5 text-xs dark:border-neutral-800 dark:bg-neutral-900"
            data-testid="reply-context"
          >
            <span class="text-indigo-500">↩</span>
            <span
              class="min-w-0 flex-1 truncate text-neutral-600 dark:text-neutral-300"
              data-testid="reply-context-preview"
            >
              {{ ws.replyContext.bodyPreview }}
            </span>
            <button
              type="button"
              class="shrink-0 rounded px-1.5 py-0.5 font-medium text-neutral-500 hover:bg-neutral-200 hover:text-neutral-800 dark:hover:bg-neutral-700 dark:hover:text-neutral-100"
              data-testid="reply-cancel"
              :aria-label="t('chat.replyCancel')"
              @click="cancelReply()"
            >
              ×
            </button>
          </div>
          <!-- M8 upload progress + inline error -->
          <div
            v-if="uploading"
            class="flex items-center gap-2 px-3 pt-2 text-[11px]"
            data-testid="upload-progress"
          >
            <div
              class="h-1 flex-1 overflow-hidden rounded bg-neutral-200 dark:bg-neutral-800"
            >
              <div
                class="h-1 rounded bg-indigo-600 transition-[width]"
                :style="{ width: uploadPercent + '%' }"
              ></div>
            </div>
            <span class="shrink-0 text-neutral-500 dark:text-neutral-400">
              {{ t("chat.mediaUploading", { percent: uploadPercent }) }}
            </span>
          </div>
          <p
            v-if="uploadError.length > 0"
            class="px-3 pt-2 text-[11px] text-red-500"
            data-testid="upload-error"
          >
            {{ uploadError }}
          </p>

          <!-- M8 emoji quick-panel popover (backdrop closes on outside click) -->
          <div
            v-if="emojiOpen"
            class="fixed inset-0 z-40"
            data-testid="emoji-backdrop"
            @click="emojiOpen = false"
          ></div>
          <div
            v-if="emojiOpen"
            class="absolute bottom-full left-3 z-50 mb-1 w-72 rounded-xl border border-neutral-200 bg-white p-2 shadow-lg dark:border-neutral-700 dark:bg-neutral-900"
            data-testid="emoji-panel"
          >
            <div class="grid max-h-56 grid-cols-8 gap-0.5 overflow-y-auto">
              <button
                v-for="emoji in EMOJIS"
                :key="emoji"
                type="button"
                class="rounded p-1 text-lg leading-none hover:bg-neutral-100 dark:hover:bg-neutral-800"
                data-testid="emoji-choice"
                @click="insertEmoji(emoji)"
              >
                {{ emoji }}
              </button>
            </div>
          </div>

          <div class="flex items-end gap-2 p-3">
            <button
              type="button"
              data-testid="emoji-toggle"
              :title="t('chat.emojiToggle')"
              :aria-label="t('chat.emojiToggle')"
              :aria-expanded="emojiOpen"
              class="shrink-0 rounded-xl px-2 py-2 text-neutral-500 hover:bg-neutral-100 hover:text-indigo-600 dark:text-neutral-400 dark:hover:bg-neutral-800 dark:hover:text-indigo-400"
              @click="toggleEmoji()"
            >
              <svg
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="2"
                stroke-linecap="round"
                stroke-linejoin="round"
                class="h-6 w-6"
                aria-hidden="true"
              >
                <circle cx="12" cy="12" r="10" />
                <path d="M8 14s1.5 2 4 2 4-2 4-2" />
                <line x1="9" y1="9" x2="9.01" y2="9" />
                <line x1="15" y1="9" x2="15.01" y2="9" />
              </svg>
            </button>
            <template v-if="mediaEnabled">
              <button
                type="button"
                data-testid="attach-image"
                :title="t('chat.mediaAttachImage')"
                :aria-label="t('chat.mediaAttachImage')"
                class="shrink-0 rounded-xl px-2 py-2 text-lg leading-none hover:bg-neutral-100 dark:hover:bg-neutral-800"
                @click="pickImage()"
              >
                🖼️
              </button>
              <button
                type="button"
                data-testid="attach-video"
                :title="t('chat.mediaAttachVideo')"
                :aria-label="t('chat.mediaAttachVideo')"
                class="shrink-0 rounded-xl px-2 py-2 text-lg leading-none hover:bg-neutral-100 dark:hover:bg-neutral-800"
                @click="pickVideo()"
              >
                🎬
              </button>
              <button
                type="button"
                data-testid="attach-audio"
                :title="t('chat.mediaAttachAudio')"
                :aria-label="t('chat.mediaAttachAudio')"
                class="shrink-0 rounded-xl px-2 py-2 text-lg leading-none hover:bg-neutral-100 dark:hover:bg-neutral-800"
                @click="pickAudio()"
              >
                🎵
              </button>
              <!-- M9 voice: record only (never in secret/e2ee chats) -->
              <button
                v-if="!recording"
                type="button"
                data-testid="voice-record"
                :title="t('chat.voiceRecord')"
                :aria-label="t('chat.voiceRecord')"
                class="shrink-0 rounded-xl px-2 py-2 text-lg leading-none hover:bg-neutral-100 dark:hover:bg-neutral-800"
                @click="startVoiceRecording()"
              >
                🎤
              </button>
            </template>
            <input
              ref="imageInputEl"
              type="file"
              accept="image/*"
              class="hidden"
              data-testid="image-file-input"
              @change="onFilePicked"
            />
            <input
              ref="videoInputEl"
              type="file"
              accept="video/*"
              class="hidden"
              data-testid="video-file-input"
              @change="onFilePicked"
            />
            <input
              ref="audioInputEl"
              type="file"
              accept="audio/*,.mp3,.m4a,.wav,.ogg,.aac,.flac"
              class="hidden"
              data-testid="audio-file-input"
              @change="onFilePicked"
            />
            <!-- Recording strip replaces the text input while capturing -->
            <template v-if="recording">
              <span
                class="flex min-h-9 flex-1 items-center gap-2 rounded-xl border border-red-300 bg-red-50 px-3 py-2 text-sm dark:border-red-800 dark:bg-red-950/40"
              >
                <span
                  class="h-2 w-2 shrink-0 animate-pulse rounded-full bg-red-500"
                  aria-hidden="true"
                ></span>
                <span
                  class="font-mono text-xs tabular-nums text-red-600 dark:text-red-300"
                  data-testid="voice-timer"
                  >{{ voiceTimer }}</span
                >
                <span class="truncate text-xs text-red-500 dark:text-red-300">
                  {{ t("chat.voiceRecording") }}
                </span>
              </span>
              <button
                type="button"
                data-testid="voice-cancel"
                :title="t('chat.voiceCancel')"
                :aria-label="t('chat.voiceCancel')"
                class="shrink-0 rounded-xl border border-neutral-300 px-3 py-2 text-sm font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
                @click="cancelVoiceRecording()"
              >
                {{ t("chat.voiceCancel") }}
              </button>
              <button
                type="button"
                data-testid="voice-stop"
                :title="t('chat.voiceStop')"
                :aria-label="t('chat.voiceStop')"
                class="shrink-0 rounded-xl bg-red-600 px-4 py-2 text-sm font-medium text-white hover:bg-red-500"
                @click="stopVoiceRecording()"
              >
                {{ t("chat.voiceStop") }}
              </button>
            </template>
            <template v-else>
              <textarea
                ref="composerEl"
                v-model="draft"
                rows="1"
                data-testid="composer-input"
                :placeholder="t('chat.inputPlaceholder')"
                :disabled="!ws.isConnected"
                class="max-h-40 min-h-9 flex-1 resize-none rounded-xl border border-neutral-300 bg-transparent px-3 py-2 text-sm outline-none focus:border-indigo-500 disabled:cursor-not-allowed disabled:opacity-60 dark:border-neutral-700"
                @keydown.enter.exact.prevent="sendMessage()"
                @input="onComposerInput()"
                @blur="onComposerBlur()"
              ></textarea>
              <button
                type="button"
                data-testid="composer-send"
                :disabled="!ws.isConnected || draft.trim().length === 0"
                class="shrink-0 rounded-xl bg-indigo-600 px-4 py-2 text-sm font-medium text-white hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-50"
                @click="sendMessage()"
              >
                {{ t("chat.composerSend") }}
              </button>
            </template>
          </div>
        </footer>
      </div>

      <!-- Forward picker modal -->
      <div
        v-if="forwardSource !== null"
        class="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
        data-testid="forward-picker"
      >
        <div
          class="w-full max-w-xs rounded-xl bg-white p-4 shadow-lg dark:bg-neutral-900"
        >
          <h3
            class="pb-2 text-sm font-semibold"
            data-testid="forward-picker-title"
          >
            {{ t("chat.forwardPickerTitle") }}
          </h3>
          <ul class="max-h-64 space-y-1 overflow-y-auto">
            <li
              v-for="conversation in forwardTargets"
              :key="conversation.conversationId"
            >
              <button
                type="button"
                class="flex w-full items-center gap-2 rounded-lg p-2 text-left text-sm hover:bg-neutral-100 dark:hover:bg-neutral-800"
                data-testid="forward-target"
                :data-conversation-id="conversation.conversationId"
                @click="confirmForward(conversation.conversationId)"
              >
                <Avatar
                  class="shrink-0"
                  :username="conversation.peerUsername"
                  :display-name="conversation.peerDisplayName"
                  :avatar="conversation.peerAvatar"
                  :size="28"
                  data-testid="forward-target-avatar"
                  @click="confirmForward(conversation.conversationId)"
                />
                {{ peerLabel(conversation) }}
              </button>
            </li>
          </ul>
          <button
            type="button"
            class="mt-2 w-full rounded-lg border border-neutral-300 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
            data-testid="forward-cancel"
            @click="forwardSource = null"
          >
            {{ t("chat.forwardPickerCancel") }}
          </button>
        </div>
      </div>

      <!-- M8 image lightbox: full-fit view, click / Esc closes -->
      <div
        v-if="lightboxMedia !== null"
        class="fixed inset-0 z-[60] flex cursor-zoom-out items-center justify-center bg-black/80 p-4"
        data-testid="media-lightbox"
        @click="closeLightbox()"
      >
        <img
          :src="mediaUrl(lightboxMedia)"
          alt=""
          class="max-h-full max-w-full object-contain"
          data-testid="media-lightbox-image"
        />
      </div>

      <!-- M11 create-group dialog: name + multi-select friends -->
      <div
        v-if="groupDialogOpen"
        class="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
        data-testid="group-create-dialog"
      >
        <div
          class="w-full max-w-sm rounded-xl bg-white p-4 shadow-lg dark:bg-neutral-900"
        >
          <h3
            class="pb-2 text-sm font-semibold"
            data-testid="group-create-title"
          >
            {{ t("group.createTitle") }}
          </h3>
          <input
            v-model="groupNameInput"
            type="text"
            :maxlength="GROUP_NAME_MAX"
            :placeholder="t('group.namePlaceholder')"
            data-testid="group-name-input"
            class="w-full rounded-lg border border-neutral-300 bg-transparent px-2 py-1.5 text-sm outline-none focus:border-indigo-500 dark:border-neutral-700"
          />
          <input
            v-model="groupMemberFilter"
            type="search"
            :placeholder="t('group.searchFriendsPlaceholder')"
            data-testid="group-member-search"
            class="mt-2 w-full rounded-lg border border-neutral-300 bg-transparent px-2 py-1.5 text-sm outline-none focus:border-indigo-500 dark:border-neutral-700"
          />
          <p
            class="px-1 pt-2 text-[11px] text-neutral-400 dark:text-neutral-500"
            data-testid="group-selected-count"
          >
            {{ t("group.selectedCount", { n: groupSelected.length }) }}
          </p>
          <ul
            v-if="groupCandidateFriends.length > 0"
            class="mt-1 max-h-56 space-y-1 overflow-y-auto"
            data-testid="group-candidate-list"
          >
            <li
              v-for="friend in groupCandidateFriends"
              :key="friend.user_id"
              data-testid="group-candidate"
              :data-username="friend.username"
            >
              <button
                type="button"
                class="flex w-full items-center gap-2 rounded-lg p-1.5 text-left text-sm hover:bg-neutral-100 dark:hover:bg-neutral-800"
                :data-selected="isGroupMemberSelected(friend.username)"
                @click="toggleGroupMember(friend.username)"
              >
                <Avatar
                  class="shrink-0"
                  :username="friend.username"
                  :display-name="friend.display_name"
                  :avatar="friend.avatar"
                  :size="28"
                />
                <span class="min-w-0 flex-1 truncate">
                  {{ friend.display_name?.trim() || friend.username }}
                </span>
                <span
                  v-if="isGroupMemberSelected(friend.username)"
                  class="shrink-0 font-semibold text-indigo-600 dark:text-indigo-400"
                  >✓</span
                >
              </button>
            </li>
          </ul>
          <p
            v-else-if="friends.loaded && friends.friends.length === 0"
            class="px-1 py-2 text-xs text-neutral-400 dark:text-neutral-500"
            data-testid="group-no-friends"
          >
            {{ t("group.noFriends") }}
          </p>
          <p
            v-if="groupCreateError.length > 0"
            class="px-1 pt-2 text-xs text-red-500"
            data-testid="group-create-error"
          >
            {{ groupCreateError }}
          </p>
          <div class="flex justify-end gap-2 pt-3">
            <button
              type="button"
              data-testid="group-create-cancel"
              class="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
              @click="closeGroupDialog()"
            >
              {{ t("group.cancel") }}
            </button>
            <button
              type="button"
              data-testid="group-create-submit"
              :disabled="!groupNameValid || creatingGroup"
              class="rounded-lg bg-indigo-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-50"
              @click="submitCreateGroup()"
            >
              {{
                creatingGroup
                  ? t("group.createCreating")
                  : t("group.createSubmit")
              }}
            </button>
          </div>
        </div>
      </div>

      <!-- M11 group info panel: right-side drawer over a frosted backdrop -->
      <Transition name="group-drawer">
        <div
          v-if="groupPanelOpen && activeGroupInfo !== null"
          class="fixed inset-0 z-50 overflow-hidden"
          data-testid="group-info-panel"
        >
          <div
            class="group-drawer-backdrop absolute inset-0 bg-black/30 backdrop-blur-md dark:bg-black/40"
            data-testid="group-info-backdrop"
            @click="closeGroupPanel()"
          ></div>
          <div
            class="group-drawer-panel absolute inset-y-0 right-0 flex h-full w-full max-w-[90vw] flex-col overflow-y-auto bg-white p-4 shadow-2xl sm:w-[400px] dark:bg-neutral-900"
            data-testid="group-info-drawer"
            role="dialog"
            aria-modal="true"
          >
            <div class="flex items-start gap-3 pb-2">
              <button
                v-if="canEditGroup"
                type="button"
                class="relative shrink-0 rounded-full"
                data-testid="group-avatar-edit"
                :title="t('group.avatarUpload')"
                :aria-label="t('group.avatarUpload')"
                @click="pickGroupAvatar()"
              >
                <Avatar
                  group
                  :username="activeGroupInfo.name"
                  :avatar="activeGroupInfo.avatar"
                  :size="44"
                />
              </button>
              <Avatar
                v-else
                group
                :username="activeGroupInfo.name"
                :avatar="activeGroupInfo.avatar"
                :size="44"
                data-testid="group-avatar"
              />
              <input
                v-if="canEditGroup"
                ref="groupAvatarInputEl"
                type="file"
                accept="image/png,image/jpeg,image/webp"
                class="hidden"
                data-testid="group-avatar-input"
                @change="onGroupAvatarFile($event)"
              />
              <div class="min-w-0 flex-1">
                <div class="flex items-center justify-between gap-2">
                  <h3
                    class="min-w-0 truncate text-sm font-semibold"
                    data-testid="group-info-title"
                  >
                    {{ activeGroupInfo.name }}
                  </h3>
                  <button
                    type="button"
                    class="shrink-0 rounded px-1.5 text-lg leading-none text-neutral-400 hover:bg-neutral-100 hover:text-neutral-700 dark:hover:bg-neutral-800"
                    data-testid="group-info-close"
                    :aria-label="t('group.close')"
                    @click="closeGroupPanel()"
                  >
                    ×
                  </button>
                </div>
                <p
                  class="text-xs text-neutral-500 dark:text-neutral-400"
                  data-testid="group-info-meta"
                >
                  {{
                    t("group.memberCount", { n: activeGroupInfo.member_count })
                  }}
                  ·
                  {{
                    t("group.myRole", {
                      role: memberRoleLabel(activeGroupInfo.my_role),
                    })
                  }}
                </p>

                <!-- M14: start a group call from the info drawer -->
                <button
                  type="button"
                  class="mt-2 flex w-full items-center justify-center gap-1.5 rounded-lg bg-indigo-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-indigo-500"
                  data-testid="group-call-start"
                  @click="startCallFromThread()"
                >
                  <svg
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="2"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                    class="h-3.5 w-3.5"
                    aria-hidden="true"
                  >
                    <path
                      d="M22 16.92v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.5 19.5 0 0 1-6-6 19.79 19.79 0 0 1-3.07-8.67A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72c.13.96.36 1.9.7 2.81a2 2 0 0 1-.45 2.11L8.09 9.91a16 16 0 0 0 6 6l1.27-1.27a2 2 0 0 1 2.11-.45c.91.34 1.85.57 2.81.7A2 2 0 0 1 22 16.92z"
                    />
                  </svg>
                  {{ t("call.groupCall") }}
                </button>

                <!-- Editable group description (owner/admin) / read-only otherwise -->
                <div v-if="groupDescriptionEditing">
                  <textarea
                    v-model="groupDescriptionDraft"
                    :maxlength="GROUP_DESCRIPTION_MAX"
                    rows="2"
                    :placeholder="t('group.descriptionPlaceholder')"
                    data-testid="group-description-input"
                    class="mt-1 w-full resize-none rounded-lg border border-neutral-300 bg-transparent px-2 py-1.5 text-xs outline-none focus:border-indigo-500 dark:border-neutral-700"
                  ></textarea>
                  <div class="mt-1 flex items-center justify-end gap-2">
                    <span
                      class="mr-auto text-[10px]"
                      :class="
                        groupDescriptionTooLong
                          ? 'text-red-500'
                          : 'text-neutral-400 dark:text-neutral-500'
                      "
                      data-testid="group-description-counter"
                      >{{
                        t("group.descriptionCounter", {
                          n: groupDescriptionDraft.length,
                        })
                      }}</span
                    >
                    <button
                      type="button"
                      data-testid="group-description-cancel"
                      class="rounded border border-neutral-300 px-2 py-0.5 text-[11px] font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
                      @click="cancelDescriptionEdit()"
                    >
                      {{ t("group.cancel") }}
                    </button>
                    <button
                      type="button"
                      data-testid="group-description-save"
                      :disabled="
                        groupDescriptionSaving || groupDescriptionTooLong
                      "
                      class="rounded bg-indigo-600 px-2 py-0.5 text-[11px] font-medium text-white hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-50"
                      @click="saveDescription()"
                    >
                      {{ t("group.titleSave") }}
                    </button>
                  </div>
                </div>
                <template v-else>
                  <p
                    v-if="groupDescriptionText.length > 0"
                    class="mt-1 whitespace-pre-wrap text-xs text-neutral-500 dark:text-neutral-400"
                    data-testid="group-description"
                  >
                    {{ groupDescriptionText }}
                  </p>
                  <p
                    v-else-if="canEditGroup"
                    class="mt-1 text-xs italic text-neutral-400 dark:text-neutral-500"
                    data-testid="group-description-empty"
                  >
                    {{ t("group.descriptionEmpty") }}
                  </p>
                  <div v-if="canEditGroup" class="mt-1 flex items-center gap-3">
                    <button
                      type="button"
                      data-testid="group-description-edit"
                      class="text-[11px] font-medium text-indigo-600 hover:underline dark:text-indigo-400"
                      @click="startDescriptionEdit()"
                    >
                      {{ t("group.editDescription") }}
                    </button>
                    <button
                      v-if="groupHasAvatar"
                      type="button"
                      data-testid="group-avatar-remove"
                      class="text-[11px] font-medium text-red-500 hover:underline"
                      @click="removeGroupAvatar()"
                    >
                      {{ t("group.avatarRemove") }}
                    </button>
                  </div>
                </template>
              </div>
            </div>

            <!-- Invite row: owner/admin only -->
            <div v-if="canInvite" class="flex items-center gap-2 pt-2">
              <input
                v-model="groupInviteUsername"
                type="text"
                :placeholder="t('group.invitePlaceholder')"
                data-testid="group-invite-input"
                class="min-w-0 flex-1 rounded-lg border border-neutral-300 bg-transparent px-2 py-1.5 text-sm outline-none focus:border-indigo-500 dark:border-neutral-700"
                @keydown.enter.prevent="submitGroupInvite()"
              />
              <button
                type="button"
                data-testid="group-invite-submit"
                :disabled="groupInviteUsername.trim().length === 0"
                class="shrink-0 rounded-lg bg-indigo-600 px-2 py-1.5 text-xs font-medium text-white hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-50"
                @click="submitGroupInvite()"
              >
                {{ t("group.invite") }}
              </button>
            </div>

            <p
              v-if="groupActionError.length > 0"
              class="px-1 pt-2 text-xs text-red-500"
              data-testid="group-action-error"
            >
              {{ groupActionError }}
            </p>

            <ul
              class="mt-2 max-h-72 space-y-1 overflow-y-auto"
              data-testid="group-member-list"
            >
              <li
                v-for="member in activeGroupInfo.members"
                :key="member.user_id"
                data-testid="group-member"
                :data-user-id="member.user_id"
                :data-role="member.role"
                class="flex items-center gap-2 rounded-lg p-1.5 hover:bg-neutral-100 dark:hover:bg-neutral-800"
              >
                <Avatar
                  class="shrink-0"
                  :username="member.username"
                  :display-name="member.display_name"
                  :avatar="member.avatar"
                  :size="28"
                />
                <span class="min-w-0 flex-1 truncate text-sm">
                  {{ memberLabelOf(member) }}
                </span>
                <span
                  v-if="memberTitleInfo(member) !== null"
                  class="shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium"
                  :class="memberTitleClass(member)"
                  data-testid="group-member-title"
                  >{{ memberTitleInfo(member)?.text }}</span
                >
                <span
                  v-if="memberLevelLabel(member) !== null"
                  class="shrink-0 text-[10px] tabular-nums text-neutral-400 dark:text-neutral-500"
                  data-testid="group-member-level"
                  >{{ memberLevelLabel(member) }}</span
                >
                <span
                  v-if="member.role === 'owner'"
                  class="shrink-0 text-sm"
                  data-testid="group-member-owner"
                  :title="t('group.roleOwner')"
                  >👑</span
                >
                <span
                  v-else-if="member.role === 'admin'"
                  class="shrink-0 rounded bg-amber-100 px-1.5 py-0.5 text-[10px] font-medium text-amber-700 dark:bg-amber-900/40 dark:text-amber-300"
                  data-testid="group-member-admin"
                  >{{ t("group.roleAdmin") }}</span
                >
                <template v-if="member.user_id !== myUserId">
                  <button
                    v-if="canSetTitle(member)"
                    type="button"
                    data-testid="group-member-title-edit"
                    :disabled="busyMemberId !== null"
                    class="shrink-0 rounded px-1.5 py-0.5 text-[11px] text-violet-600 hover:underline disabled:opacity-50 dark:text-violet-400"
                    @click="openTitleDialog(member)"
                  >
                    {{ t("group.setTitle") }}
                  </button>
                  <button
                    v-if="
                      canChangeMemberRole(member) && member.role !== 'admin'
                    "
                    type="button"
                    data-testid="group-appoint"
                    :disabled="busyMemberId !== null"
                    class="shrink-0 rounded px-1.5 py-0.5 text-[11px] text-indigo-600 hover:underline disabled:opacity-50 dark:text-indigo-400"
                    @click="askRoleChange(member, 'admin')"
                  >
                    {{ t("group.appointAdmin") }}
                  </button>
                  <button
                    v-if="
                      canChangeMemberRole(member) && member.role === 'admin'
                    "
                    type="button"
                    data-testid="group-demote"
                    :disabled="busyMemberId !== null"
                    class="shrink-0 rounded px-1.5 py-0.5 text-[11px] text-indigo-600 hover:underline disabled:opacity-50 dark:text-indigo-400"
                    @click="askRoleChange(member, 'member')"
                  >
                    {{ t("group.demoteAdmin") }}
                  </button>
                  <button
                    v-if="canTransferTo(member)"
                    type="button"
                    data-testid="group-transfer"
                    :disabled="busyMemberId !== null"
                    class="shrink-0 rounded px-1.5 py-0.5 text-[11px] text-neutral-500 hover:underline disabled:opacity-50 dark:text-neutral-400"
                    @click="askTransfer(member)"
                  >
                    {{ t("group.transfer") }}
                  </button>
                  <button
                    v-if="canKick(member)"
                    type="button"
                    data-testid="group-kick"
                    :disabled="busyMemberId !== null"
                    class="shrink-0 rounded px-1.5 py-0.5 text-[11px] text-red-500 hover:underline disabled:opacity-50"
                    @click="askKick(member)"
                  >
                    {{ t("group.kick") }}
                  </button>
                </template>
                <span
                  v-else
                  class="shrink-0 text-[10px] text-neutral-400 dark:text-neutral-500"
                  data-testid="group-member-self"
                  >{{ t("group.you") }}</span
                >
              </li>
            </ul>

            <!-- M13b group files: usage, upload + drag & drop, listing -->
            <section
              class="relative mt-3 rounded-xl border border-neutral-200 p-2 dark:border-neutral-800"
              :class="
                fileDragActive
                  ? 'border-dashed border-indigo-400 bg-indigo-50/60 dark:bg-indigo-950/20'
                  : ''
              "
              data-testid="group-file-dropzone"
              @dragenter.prevent="onFileDragEnter"
              @dragover.prevent="onFileDragOver"
              @dragleave.prevent="onFileDragLeave"
              @drop.prevent="onFileDrop"
            >
              <div class="flex items-center justify-between gap-2">
                <h4
                  class="text-xs font-semibold text-neutral-700 dark:text-neutral-200"
                  data-testid="group-file-title"
                >
                  {{ t("group.filesTitle") }}
                </h4>
                <button
                  type="button"
                  class="shrink-0 rounded-lg bg-indigo-600 px-2 py-1 text-[11px] font-medium text-white hover:bg-indigo-500"
                  data-testid="group-file-upload"
                  @click="pickGroupFile()"
                >
                  {{ t("group.filesUpload") }}
                </button>
                <input
                  ref="groupFileInputEl"
                  type="file"
                  multiple
                  class="hidden"
                  data-testid="group-file-input"
                  @change="onGroupFilePicked($event)"
                />
              </div>

              <!-- Usage bar: used / quota + over-quota hint -->
              <div class="pt-2" data-testid="group-file-usage">
                <p class="text-[11px] text-neutral-500 dark:text-neutral-400">
                  {{
                    t("group.filesUsage", {
                      used: formatBytes(groupFileUsage),
                      quota: formatBytes(groupFileQuota),
                    })
                  }}
                </p>
                <div
                  class="mt-1 h-1 overflow-hidden rounded bg-neutral-200 dark:bg-neutral-800"
                >
                  <div
                    class="h-1 rounded bg-indigo-600 transition-[width]"
                    :style="{ width: groupFileUsagePercent + '%' }"
                  ></div>
                </div>
                <p
                  v-if="groupFileOverQuota"
                  class="pt-1 text-[10px] text-amber-600 dark:text-amber-400"
                  data-testid="group-file-over-quota"
                >
                  {{ t("group.filesOverQuota") }}
                </p>
              </div>

              <!-- Upload progress (scoped to the drawer) -->
              <div
                v-if="fileUploading"
                class="flex items-center gap-2 pt-2 text-[11px]"
                data-testid="group-file-upload-progress"
              >
                <div
                  class="h-1 flex-1 overflow-hidden rounded bg-neutral-200 dark:bg-neutral-800"
                >
                  <div
                    class="h-1 rounded bg-indigo-600"
                    :style="{ width: fileUploadPercent + '%' }"
                  ></div>
                </div>
                <span class="shrink-0 text-neutral-500 dark:text-neutral-400">
                  {{
                    t("group.filesUploading", { percent: fileUploadPercent })
                  }}
                </span>
              </div>

              <p
                v-if="fileError.length > 0"
                class="pt-2 text-[11px] text-red-500"
                data-testid="group-file-error"
              >
                {{ fileError }}
              </p>

              <p
                v-if="groupFileList.length === 0"
                class="pt-2 text-[11px] italic text-neutral-400 dark:text-neutral-500"
                data-testid="group-file-empty"
              >
                {{ t("group.filesEmpty") }}
              </p>

              <ul
                v-else
                class="mt-2 max-h-56 space-y-1 overflow-y-auto"
                data-testid="group-file-list"
              >
                <li
                  v-for="file in groupFileList"
                  :key="file.file_id"
                  class="flex items-center gap-2 rounded-lg p-1.5 hover:bg-neutral-100 dark:hover:bg-neutral-800"
                  data-testid="group-file"
                  :data-file-id="file.file_id"
                >
                  <svg
                    class="h-5 w-5 shrink-0 text-neutral-400 dark:text-neutral-500"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="1.8"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                    aria-hidden="true"
                  >
                    <template v-if="groupFileIconKind(file) === 'image'">
                      <rect x="3" y="3" width="18" height="18" rx="2" />
                      <circle cx="8.5" cy="8.5" r="1.5" />
                      <path d="m21 15-5-5L5 21" />
                    </template>
                    <template v-else-if="groupFileIconKind(file) === 'video'">
                      <rect x="2" y="5" width="14" height="14" rx="2" />
                      <path d="m16 10 6-3v10l-6-3z" />
                    </template>
                    <template v-else-if="groupFileIconKind(file) === 'audio'">
                      <path d="M9 18V5l12-2v13" />
                      <circle cx="6" cy="18" r="3" />
                      <circle cx="18" cy="16" r="3" />
                    </template>
                    <template v-else-if="groupFileIconKind(file) === 'pdf'">
                      <path
                        d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"
                      />
                      <path d="M14 2v6h6" />
                      <path d="M9 13h1.5a1.5 1.5 0 0 1 0 3H9v2" />
                    </template>
                    <template v-else-if="groupFileIconKind(file) === 'zip'">
                      <path
                        d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"
                      />
                      <path d="M14 2v6h6" />
                      <path d="M12 4v2M12 9v2M12 14v2" />
                    </template>
                    <template v-else-if="groupFileIconKind(file) === 'text'">
                      <path
                        d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"
                      />
                      <path d="M14 2v6h6" />
                      <path d="M8 13h8M8 17h5" />
                    </template>
                    <template v-else>
                      <path
                        d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"
                      />
                      <path d="M14 2v6h6" />
                    </template>
                  </svg>
                  <div class="min-w-0 flex-1">
                    <p
                      class="truncate text-xs text-neutral-800 dark:text-neutral-100"
                      data-testid="group-file-name"
                      :title="file.name"
                    >
                      {{ file.name }}
                    </p>
                    <p
                      class="flex items-center gap-1.5 text-[10px] text-neutral-400 dark:text-neutral-500"
                    >
                      <span data-testid="group-file-size">{{
                        formatBytes(file.bytes)
                      }}</span>
                      <span>·</span>
                      <span
                        class="truncate"
                        data-testid="group-file-uploader"
                        >{{
                          file.uploader.display_name?.trim() ||
                          file.uploader.username
                        }}</span
                      >
                      <span>·</span>
                      <span data-testid="group-file-date">{{
                        groupFileDate(file.created_at)
                      }}</span>
                      <span
                        v-if="file.expires_at"
                        class="shrink-0 rounded bg-amber-100 px-1 py-0.5 font-medium text-amber-700 dark:bg-amber-900/40 dark:text-amber-300"
                        data-testid="group-file-expiry"
                        :title="groupFileDate(file.expires_at)"
                        >{{ t("group.filesExpiryBadge") }}</span
                      >
                    </p>
                  </div>
                  <button
                    type="button"
                    class="shrink-0 rounded px-1.5 py-0.5 text-[11px] text-indigo-600 hover:underline disabled:opacity-50 dark:text-indigo-400"
                    data-testid="group-file-download"
                    :disabled="fileBusyId !== null"
                    @click="downloadGroupFileAction(file)"
                  >
                    {{ t("group.filesDownload") }}
                  </button>
                  <button
                    v-if="canDeleteGroupFile(file)"
                    type="button"
                    class="shrink-0 rounded px-1.5 py-0.5 text-[11px] text-red-500 hover:underline disabled:opacity-50"
                    data-testid="group-file-delete"
                    :disabled="fileBusyId !== null"
                    @click="askDeleteGroupFile(file)"
                  >
                    {{ t("group.filesDelete") }}
                  </button>
                </li>
              </ul>

              <!-- Drop hint overlay (pointer-events-none keeps dragleave stable) -->
              <div
                v-if="fileDragActive"
                class="pointer-events-none absolute inset-0 z-10 flex items-center justify-center rounded-xl border-2 border-dashed border-indigo-400 bg-white/85 text-xs font-medium text-indigo-600 dark:bg-neutral-900/85 dark:text-indigo-300"
                data-testid="group-file-drop-hint"
              >
                {{ t("group.filesDropHint") }}
              </div>
            </section>

            <!-- Confirm overlay for deleting a group file -->
            <div
              v-if="fileDeleteTarget !== null"
              class="fixed inset-0 z-[60] flex items-center justify-center bg-black/40 p-4"
              data-testid="group-file-delete-confirm"
            >
              <div
                class="w-full max-w-xs rounded-xl bg-white p-4 shadow-lg dark:bg-neutral-900"
              >
                <p
                  class="pb-3 text-sm text-neutral-700 dark:text-neutral-200"
                  data-testid="group-file-delete-message"
                >
                  {{
                    t("group.filesDeleteConfirm", {
                      name: fileDeleteTarget.name,
                    })
                  }}
                </p>
                <div class="flex justify-end gap-2">
                  <button
                    type="button"
                    data-testid="group-file-delete-cancel"
                    class="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
                    @click="cancelDeleteGroupFile()"
                  >
                    {{ t("group.cancel") }}
                  </button>
                  <button
                    type="button"
                    data-testid="group-file-delete-accept"
                    class="rounded-lg bg-red-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-red-500"
                    @click="confirmDeleteGroupFile()"
                  >
                    {{ t("group.confirm") }}
                  </button>
                </div>
              </div>
            </div>

            <div class="pt-3">
              <button
                v-if="canLeave"
                type="button"
                data-testid="group-leave"
                class="w-full rounded-lg bg-red-600 py-1.5 text-xs font-medium text-white hover:bg-red-500"
                @click="askLeave()"
              >
                {{ t("group.leave") }}
              </button>
              <p
                v-else
                class="px-1 text-[11px] text-neutral-400 dark:text-neutral-500"
                data-testid="group-owner-leave-hint"
              >
                {{ t("group.ownerLeaveHint") }}
              </p>
            </div>

            <!-- In-app confirmation overlay for destructive group actions -->
            <div
              v-if="groupConfirm !== null"
              class="fixed inset-0 z-[60] flex items-center justify-center bg-black/40 p-4"
              data-testid="group-confirm"
            >
              <div
                class="w-full max-w-xs rounded-xl bg-white p-4 shadow-lg dark:bg-neutral-900"
              >
                <p
                  class="pb-3 text-sm text-neutral-700 dark:text-neutral-200"
                  data-testid="group-confirm-message"
                >
                  {{ groupConfirm.message }}
                </p>
                <div class="flex justify-end gap-2">
                  <button
                    type="button"
                    data-testid="group-confirm-cancel"
                    class="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
                    @click="cancelGroupConfirm()"
                  >
                    {{ t("group.cancel") }}
                  </button>
                  <button
                    type="button"
                    data-testid="group-confirm-accept"
                    class="rounded-lg bg-red-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-red-500"
                    @click="runGroupConfirm()"
                  >
                    {{ t("group.confirm") }}
                  </button>
                </div>
              </div>
            </div>

            <!-- M12b member-title dialog (owner only, never on the owner) -->
            <div
              v-if="titleDialogMember !== null"
              class="fixed inset-0 z-[60] flex items-center justify-center bg-black/40 p-4"
              data-testid="group-title-dialog"
            >
              <div
                class="w-full max-w-xs rounded-xl bg-white p-4 shadow-lg dark:bg-neutral-900"
              >
                <h3
                  class="pb-2 text-sm font-semibold"
                  data-testid="group-title-title"
                >
                  {{ t("group.setTitle") }}
                </h3>
                <p class="pb-1 text-xs text-neutral-500 dark:text-neutral-400">
                  {{ memberLabelOf(titleDialogMember) }}
                </p>
                <input
                  v-model="titleDraft"
                  type="text"
                  :maxlength="GROUP_TITLE_MAX"
                  :placeholder="t('group.titlePlaceholder')"
                  data-testid="group-title-input"
                  class="w-full rounded-lg border border-neutral-300 bg-transparent px-2 py-1.5 text-sm outline-none focus:border-indigo-500 dark:border-neutral-700"
                />
                <p
                  class="pt-1 text-right text-[10px] text-neutral-400 dark:text-neutral-500"
                >
                  {{ t("group.titleCounter", { n: titleDraft.length }) }}
                </p>
                <div class="flex justify-end gap-2 pt-2">
                  <button
                    type="button"
                    data-testid="group-title-clear"
                    :disabled="titleSaving"
                    class="mr-auto rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-red-500 hover:bg-neutral-100 disabled:opacity-50 dark:border-neutral-700 dark:hover:bg-neutral-800"
                    @click="clearTitle()"
                  >
                    {{ t("group.titleClear") }}
                  </button>
                  <button
                    type="button"
                    data-testid="group-title-cancel"
                    class="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
                    @click="closeTitleDialog()"
                  >
                    {{ t("group.cancel") }}
                  </button>
                  <button
                    type="button"
                    data-testid="group-title-save"
                    :disabled="titleSaving"
                    class="rounded-lg bg-indigo-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-50"
                    @click="saveTitle()"
                  >
                    {{ t("group.titleSave") }}
                  </button>
                </div>
              </div>
            </div>
          </div>
        </div>
      </Transition>

      <!-- M12b app settings: centered modal over a frosted backdrop -->
      <div
        v-if="settingsOpen"
        class="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4 backdrop-blur-md"
        data-testid="settings-dialog"
        @click.self="closeSettings()"
      >
        <div
          class="w-full max-w-xs rounded-xl bg-white p-4 shadow-lg dark:bg-neutral-900"
          role="dialog"
          aria-modal="true"
        >
          <div class="flex items-center justify-between pb-2">
            <h3 class="text-sm font-semibold" data-testid="settings-title">
              {{ t("settings.title") }}
            </h3>
            <button
              type="button"
              class="shrink-0 rounded px-1.5 text-lg leading-none text-neutral-400 hover:bg-neutral-100 hover:text-neutral-700 dark:hover:bg-neutral-800"
              data-testid="settings-close"
              :aria-label="t('settings.title')"
              @click="closeSettings()"
            >
              ×
            </button>
          </div>

          <!-- Theme -->
          <div class="pt-1">
            <p
              class="pb-1 text-xs font-medium text-neutral-500 dark:text-neutral-400"
            >
              {{ t("settings.theme") }}
            </p>
            <div
              class="flex overflow-hidden rounded-lg border border-neutral-300 text-xs dark:border-neutral-700"
            >
              <button
                v-for="option in themeOptions"
                :key="option.value"
                type="button"
                class="flex-1 px-2 py-1.5 font-medium transition-colors"
                :class="
                  themePreference === option.value
                    ? 'bg-indigo-600 text-white'
                    : 'text-neutral-500 hover:bg-neutral-100 dark:text-neutral-400 dark:hover:bg-neutral-800'
                "
                :data-testid="`settings-theme-${option.value}`"
                @click="setTheme(option.value)"
              >
                {{ option.label }}
              </button>
            </div>
          </div>

          <!-- Language -->
          <div class="pt-3">
            <p
              class="pb-1 text-xs font-medium text-neutral-500 dark:text-neutral-400"
            >
              {{ t("settings.language") }}
            </p>
            <div
              class="flex overflow-hidden rounded-lg border border-neutral-300 text-xs dark:border-neutral-700"
            >
              <button
                type="button"
                class="flex-1 px-2 py-1.5 font-medium transition-colors"
                :class="
                  locale === 'zh-CN'
                    ? 'bg-indigo-600 text-white'
                    : 'text-neutral-500 hover:bg-neutral-100 dark:text-neutral-400 dark:hover:bg-neutral-800'
                "
                data-testid="settings-locale-zh"
                @click="setLocale('zh-CN')"
              >
                中文
              </button>
              <button
                type="button"
                class="flex-1 border-l border-neutral-300 px-2 py-1.5 font-medium transition-colors dark:border-neutral-700"
                :class="
                  locale === 'en'
                    ? 'bg-indigo-600 text-white'
                    : 'text-neutral-500 hover:bg-neutral-100 dark:text-neutral-400 dark:hover:bg-neutral-800'
                "
                data-testid="settings-locale-en"
                @click="setLocale('en')"
              >
                English
              </button>
            </div>
          </div>

          <p
            class="pt-3 text-[11px] text-neutral-400 dark:text-neutral-500"
            data-testid="settings-version"
          >
            {{ t("settings.version", { version: appVersion }) }}
          </p>
          <p
            class="pt-1 text-[11px] text-neutral-500 dark:text-neutral-400"
            data-testid="settings-sponsor"
          >
            {{ t("settings.sponsor") }}
          </p>
        </div>
      </div>

      <!-- M14 incoming call dialog -->
      <div
        v-if="calls.status === 'incoming'"
        class="fixed inset-0 z-[70] flex items-center justify-center bg-black/50 p-4 backdrop-blur-md"
        data-testid="incoming-call"
      >
        <div
          class="w-full max-w-xs rounded-2xl bg-white p-5 text-center shadow-2xl dark:bg-neutral-900"
          role="dialog"
          aria-modal="true"
        >
          <div class="flex justify-center">
            <Avatar
              :username="calls.peer?.username ?? ''"
              :display-name="calls.peer?.displayName"
              :avatar="null"
              :size="56"
            />
          </div>
          <p class="mt-2 text-sm font-semibold" data-testid="incoming-caller">
            {{ incomingCallerName }}
          </p>
          <p class="text-xs text-neutral-500 dark:text-neutral-400">
            {{ t("call.incoming") }}
          </p>
          <div class="mt-4 flex justify-center gap-6">
            <button
              type="button"
              class="flex h-11 w-11 items-center justify-center rounded-full bg-red-600 text-white hover:bg-red-500"
              data-testid="incoming-reject"
              :title="t('call.reject')"
              :aria-label="t('call.reject')"
              @click="rejectCall()"
            >
              <svg
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="2"
                stroke-linecap="round"
                stroke-linejoin="round"
                class="h-5 w-5"
                aria-hidden="true"
                transform="rotate(135 12 12)"
              >
                <path
                  d="M22 16.92v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.5 19.5 0 0 1-6-6 19.79 19.79 0 0 1-3.07-8.67A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72c.13.96.36 1.9.7 2.81a2 2 0 0 1-.45 2.11L8.09 9.91a16 16 0 0 0 6 6l1.27-1.27a2 2 0 0 1 2.11-.45c.91.34 1.85.57 2.81.7A2 2 0 0 1 22 16.92z"
                />
              </svg>
            </button>
            <button
              type="button"
              class="flex h-11 w-11 items-center justify-center rounded-full bg-emerald-600 text-white hover:bg-emerald-500"
              data-testid="incoming-accept"
              :title="t('call.accept')"
              :aria-label="t('call.accept')"
              @click="acceptCall()"
            >
              <svg
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="2"
                stroke-linecap="round"
                stroke-linejoin="round"
                class="h-5 w-5"
                aria-hidden="true"
              >
                <path
                  d="M22 16.92v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.5 19.5 0 0 1-6-6 19.79 19.79 0 0 1-3.07-8.67A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72c.13.96.36 1.9.7 2.81a2 2 0 0 1-.45 2.11L8.09 9.91a16 16 0 0 0 6 6l1.27-1.27a2 2 0 0 1 2.11-.45c.91.34 1.85.57 2.81.7A2 2 0 0 1 22 16.92z"
                />
              </svg>
            </button>
          </div>
        </div>
      </div>

      <!-- M14 call toast (ended / rejected / busy / mic / share errors) -->
      <div
        v-if="calls.toast !== null"
        class="fixed bottom-6 left-1/2 z-[80] -translate-x-1/2 rounded-full px-4 py-2 text-xs font-medium text-white shadow-lg"
        :class="calls.toast.tone === 'error' ? 'bg-red-600' : 'bg-neutral-800'"
        data-testid="call-toast"
      >
        {{ t(calls.toast.key) }}
      </div>
    </template>
  </AppShell>
</template>

<style scoped>
/* Group info drawer: slide in/out from the right edge on a strong ease-out
   curve, with the dim layer fading in behind the frosted-glass backdrop. */
.group-drawer-enter-active .group-drawer-panel,
.group-drawer-leave-active .group-drawer-panel {
  transition: transform 320ms cubic-bezier(0.22, 1, 0.36, 1);
}
.group-drawer-enter-active .group-drawer-backdrop,
.group-drawer-leave-active .group-drawer-backdrop {
  transition: opacity 320ms cubic-bezier(0.22, 1, 0.36, 1);
}
.group-drawer-enter-from .group-drawer-panel,
.group-drawer-leave-to .group-drawer-panel {
  transform: translateX(100%);
}
.group-drawer-enter-from .group-drawer-backdrop,
.group-drawer-leave-to .group-drawer-backdrop {
  opacity: 0;
}

/* Reduced motion: cut the slide/fade down to a near-instant swap. */
@media (prefers-reduced-motion: reduce) {
  .group-drawer-enter-active .group-drawer-panel,
  .group-drawer-leave-active .group-drawer-panel,
  .group-drawer-enter-active .group-drawer-backdrop,
  .group-drawer-leave-active .group-drawer-backdrop {
    transition-duration: 0.01ms;
  }
}
</style>
