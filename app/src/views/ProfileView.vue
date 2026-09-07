<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRoute, useRouter } from "vue-router";
import Avatar from "../components/Avatar.vue";
import { AVATAR_EMOJIS } from "../lib/avatars";
import { displayNameOf } from "../lib/identity";
import { bandOf } from "../lib/levels";
import type { RankKey } from "../lib/levels";
import { PROFILE_LIMITS } from "../lib/api/profile";
import type { UserProfile } from "../lib/api/profile";
import { useAuthStore } from "../stores/auth";
import { useProfileStore } from "../stores/profile";

const { t } = useI18n();
const route = useRoute();
const router = useRouter();
const auth = useAuthStore();
const profile = useProfileStore();

/** Route param userId (null on /profile without a param). */
const peerId = computed<string | null>(() =>
  typeof route.params.userId === "string" ? route.params.userId : null,
);

/** Self mode when no peer param is given, or the param IS the signed-in user. */
const isSelf = computed<boolean>(
  () =>
    peerId.value === null ||
    (auth.user !== null && peerId.value === auth.user.userId),
);

const uid = computed<number>(() => {
  const profileUid = viewed.value.uid;
  if (typeof profileUid === "number" && profileUid > 0) return profileUid;
  return auth.user?.uid ?? 0;
});

/**
 * The profile to render. For self mode it is the profile-store cache or the
 * auth seed; for peer mode it is the seeded/cached peer profile. All fields
 * come from the server when available — this object is display-only.
 */
const viewed = computed<UserProfile>(() => {
  if (isSelf.value) {
    return profile.selfProfile;
  }
  const seed = profile.peerProfile(peerId.value ?? "", {
    username: route.query.username as string | undefined,
    uid: Number(route.query.uid) || 0,
    displayName: route.query.displayName as string | undefined,
    avatar: route.query.avatar as string | null | undefined,
  });
  return seed;
});

/** Editing capability: self mode only. */
const editing = ref(false);
/** Avatar picker open state. */
const pickerOpen = ref(false);
const saving = ref(false);

/** Draft form state (populated when entering edit mode). */
const draftName = ref("");
const draftBio = ref("");
const draftAvatar = ref<string | null>(null);

const bioTooLong = computed<boolean>(
  () => draftBio.value.length > PROFILE_LIMITS.bioMax,
);
const nameTooLong = computed<boolean>(
  () => draftName.value.length > PROFILE_LIMITS.displayNameMax,
);

/** Rank band for the level chip (color dot + localized label). */
const rankBand = computed(() => bandOf(viewed.value.level));

const titleLabel = computed<string>(() =>
  t(
    `profile.rankTitles.${rankBand.value.key}` as `profile.rankTitles.${RankKey}`,
  ),
);

const xpLabel = computed<string>(() =>
  t("profile.xpLabel", { level: viewed.value.level }),
);

const xpToNextLabel = computed<string>(() =>
  t("profile.xpToNext", {
    xp: viewed.value.xp,
    xpToNext: viewed.value.xp_to_next,
  }),
);

/** Percentage width of the level progress bar (clamped 0-100). */
const progressPct = computed<number>(() => {
  const total = viewed.value.xp_to_next;
  if (!(total > 0)) return 0;
  const pct = Math.round((viewed.value.xp / total) * 100);
  return Math.min(100, Math.max(0, pct));
});

const remainingXp = computed<number>(() =>
  Math.max(0, viewed.value.xp_to_next - viewed.value.xp),
);

function startEditing(): void {
  draftName.value = viewed.value.display_name ?? "";
  draftBio.value = viewed.value.bio ?? "";
  draftAvatar.value = viewed.value.avatar ?? null;
  editing.value = true;
}

function cancelEditing(): void {
  editing.value = false;
  pickerOpen.value = false;
}

function selectAvatar(emoji: string): void {
  draftAvatar.value = emoji;
  pickerOpen.value = false;
}

async function saveEdits(): Promise<void> {
  const name = draftName.value.trim();
  if (bioTooLong.value || nameTooLong.value || name.length === 0) return;
  if (saving.value) return;
  saving.value = true;
  try {
    await profile.saveMe({
      display_name: name.length > 0 ? name : undefined,
      bio: draftBio.value.trim(),
      avatar: draftAvatar.value ?? undefined,
    });
    editing.value = false;
  } catch {
    // Error surfaced via store's patchError; keep the form open.
  } finally {
    saving.value = false;
  }
}

const saveError = computed<string>(() => profile.patchError ?? "");

function backToChat(): void {
  void router.push("/chat");
}

onMounted(() => {
  if (isSelf.value) {
    profile.ensureLoaded();
  } else {
    const userId = peerId.value;
    if (userId !== null) void profile.fetchPeer(userId);
  }
});

watch(peerId, () => {
  if (isSelf.value) {
    profile.ensureLoaded();
  } else if (peerId.value !== null) {
    void profile.fetchPeer(peerId.value);
  }
  cancelEditing();
});
</script>

<template>
  <div class="flex h-full flex-col items-center overflow-y-auto p-6">
    <!-- Back to chat affordance -->
    <div class="mb-4 flex w-full max-w-md justify-between">
      <button
        type="button"
        class="text-xs font-medium text-neutral-500 hover:text-neutral-900 dark:text-neutral-400 dark:hover:text-white"
        data-testid="profile-back"
        @click="backToChat()"
      >
        ← {{ t("profile.backToChat") }}
      </button>
    </div>

    <!-- Header: avatar (self → opens the picker) + names + chips -->
    <div
      class="flex w-full max-w-md flex-col items-center rounded-2xl border border-neutral-200 p-6 dark:border-neutral-800"
      data-testid="profile-card"
    >
      <button
        v-if="isSelf"
        type="button"
        class="relative"
        data-testid="profile-avatar-self"
        :title="t('profile.avatarPickerTitle')"
        @click="pickerOpen = true"
      >
        <Avatar
          :username="viewed.username"
          :display-name="viewed.display_name"
          :avatar="viewed.avatar"
          :size="96"
          @click="pickerOpen = true"
        />
        <span
          class="absolute -bottom-1 -right-1 flex h-7 w-7 items-center justify-center rounded-full bg-indigo-600 text-xs text-white shadow"
          >📷</span
        >
      </button>
      <Avatar
        v-else
        :username="viewed.username"
        :display-name="viewed.display_name"
        :avatar="viewed.avatar"
        :size="96"
      />

      <h1
        class="mt-3 text-lg font-bold text-neutral-900 dark:text-white"
        data-testid="profile-display-name"
      >
        {{ displayNameOf(viewed) }}
      </h1>
      <p class="font-mono text-xs text-neutral-400 dark:text-neutral-500">
        @{{ viewed.username }}
        <template v-if="uid > 0"> · UID {{ uid }}</template>
      </p>

      <div class="mt-3 flex items-center gap-2">
        <span
          class="inline-flex items-center gap-1 rounded-full bg-indigo-50 px-2.5 py-0.5 text-xs font-semibold text-indigo-700 dark:bg-indigo-900/50 dark:text-indigo-300"
          data-testid="profile-level-chip"
        >
          {{ xpLabel }}
        </span>
        <span
          class="inline-flex items-center gap-1.5 rounded-full bg-neutral-100 px-2.5 py-0.5 text-xs font-semibold text-neutral-700 dark:bg-neutral-800 dark:text-neutral-200"
          data-testid="profile-title-chip"
        >
          <span
            class="inline-block h-2.5 w-2.5 rounded-sm"
            :style="{ backgroundColor: rankBand.color }"
            data-testid="profile-title-dot"
          ></span>
          {{ titleLabel }}
        </span>
      </div>

      <!-- Level progress bar -->
      <div class="mt-4 w-full" data-testid="profile-level-bar">
        <div
          class="h-2 w-full overflow-hidden rounded-full bg-neutral-200 dark:bg-neutral-700"
        >
          <div
            class="h-full rounded-full bg-indigo-500 transition-all"
            :style="{ width: `${progressPct}%` }"
            data-testid="profile-level-progress"
          ></div>
        </div>
        <p
          class="mt-1 text-right text-[11px] text-neutral-400 dark:text-neutral-500"
        >
          {{ xpToNextLabel }}
          <template v-if="remainingXp > 0">
            · {{ t("profile.xpRemaining", { remain: remainingXp }) }}
          </template>
        </p>
      </div>

      <!-- Bio -->
      <p
        v-if="!editing && viewed.bio.length > 0"
        class="mt-4 w-full text-center text-sm leading-relaxed text-neutral-500 dark:text-neutral-400"
        data-testid="profile-bio"
      >
        {{ viewed.bio }}
      </p>
    </div>

    <!-- Edit mode (self only) -->
    <form
      v-if="isSelf && editing"
      class="mt-4 flex w-full max-w-md flex-col gap-3"
      data-testid="profile-edit-form"
      @submit.prevent="saveEdits()"
    >
      <label class="flex flex-col gap-1">
        <span
          class="text-xs font-medium text-neutral-500 dark:text-neutral-400"
        >
          {{ t("profile.displayNameLabel") }}
        </span>
        <input
          v-model="draftName"
          type="text"
          :maxlength="PROFILE_LIMITS.displayNameMax"
          data-testid="profile-name-input"
          :placeholder="t('profile.displayNamePlaceholder')"
          class="rounded-lg border border-neutral-300 bg-transparent px-3 py-2 text-sm outline-none focus:border-indigo-500 dark:border-neutral-700"
        />
        <span
          v-if="nameTooLong"
          class="text-[11px] text-red-500"
          data-testid="profile-name-error"
        >
          {{ t("profile.errors.nameTooLong") }}
        </span>
      </label>

      <label class="flex flex-col gap-1">
        <span
          class="text-xs font-medium text-neutral-500 dark:text-neutral-400"
        >
          {{ t("profile.bioLabel") }}
        </span>
        <textarea
          v-model="draftBio"
          :maxlength="PROFILE_LIMITS.bioMax"
          rows="3"
          data-testid="profile-bio-input"
          :placeholder="t('profile.bioPlaceholder')"
          class="resize-none rounded-lg border border-neutral-300 bg-transparent px-3 py-2 text-sm outline-none focus:border-indigo-500 dark:border-neutral-700"
        ></textarea>
        <span class="self-end text-[11px]" data-testid="profile-bio-counter">
          <span
            :class="
              bioTooLong
                ? 'text-red-500'
                : 'text-neutral-400 dark:text-neutral-500'
            "
            >{{ t("profile.bioCounter", { n: draftBio.length }) }}</span
          >
        </span>
        <span
          v-if="bioTooLong"
          class="text-[11px] text-red-500"
          data-testid="profile-bio-error"
        >
          {{ t("profile.errors.bioTooLong") }}
        </span>
      </label>

      <!-- Current draft avatar + change button (reuses the picker) -->
      <div class="flex items-center gap-3">
        <Avatar
          :username="viewed.username"
          :display-name="viewed.display_name"
          :avatar="draftAvatar"
          :size="48"
        />
        <button
          type="button"
          data-testid="profile-pick-avatar"
          class="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
          @click="pickerOpen = true"
        >
          {{ t("profile.avatarPickerTitle") }}
        </button>
      </div>

      <p
        v-if="saveError.length > 0"
        class="text-xs text-red-500"
        data-testid="profile-save-error"
      >
        {{ saveError }}
      </p>

      <div class="flex justify-end gap-2">
        <button
          type="button"
          data-testid="profile-cancel"
          class="rounded-lg border border-neutral-300 px-3 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
          @click="cancelEditing()"
        >
          {{ t("profile.editCancel") }}
        </button>
        <button
          type="submit"
          :disabled="saving || bioTooLong || nameTooLong"
          data-testid="profile-save"
          class="rounded-lg bg-indigo-600 px-4 py-1.5 text-xs font-medium text-white hover:bg-indigo-500 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {{ saving ? "…" : t("profile.editSave") }}
        </button>
      </div>
    </form>

    <!-- Self mode, not editing: 编辑资料 button -->
    <button
      v-else-if="isSelf"
      type="button"
      data-testid="profile-edit"
      class="mt-4 rounded-lg bg-indigo-600 px-4 py-1.5 text-xs font-medium text-white hover:bg-indigo-500"
      @click="startEditing()"
    >
      {{ t("profile.editTitle") }}
    </button>

    <!-- Avatar picker modal (self only) -->
    <div
      v-if="pickerOpen"
      class="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
      data-testid="profile-avatar-picker"
    >
      <div
        class="w-full max-w-sm rounded-xl bg-white p-4 shadow-lg dark:bg-neutral-900"
      >
        <h3
          class="pb-1 text-sm font-semibold"
          data-testid="avatar-picker-title"
        >
          {{ t("profile.avatarPickerTitle") }}
        </h3>
        <p class="pb-3 text-[11px] text-neutral-400 dark:text-neutral-500">
          {{ t("profile.avatarPickerHint") }}
        </p>
        <div class="grid grid-cols-6 gap-1">
          <button
            v-for="emoji in AVATAR_EMOJIS"
            :key="emoji"
            type="button"
            class="flex h-10 w-10 items-center justify-center rounded-lg text-xl hover:bg-neutral-100 dark:hover:bg-neutral-800"
            :class="draftAvatar === emoji ? 'ring-2 ring-indigo-500' : ''"
            data-testid="avatar-choice"
            @click="selectAvatar(emoji)"
          >
            {{ emoji }}
          </button>
        </div>
        <button
          type="button"
          class="mt-3 w-full rounded-lg border border-neutral-300 py-1.5 text-xs font-medium text-neutral-600 hover:bg-neutral-100 dark:border-neutral-700 dark:text-neutral-300 dark:hover:bg-neutral-800"
          data-testid="avatar-picker-cancel"
          @click="pickerOpen = false"
        >
          {{ t("profile.editCancel") }}
        </button>
      </div>
    </div>
  </div>
</template>
