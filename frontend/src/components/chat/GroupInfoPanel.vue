<script setup lang="ts">
import { computed, ref } from "vue";
import type { ConversationSummary } from "../../generated/ConversationSummary";
import type { MemberView } from "../../generated/MemberView";
import type { Role } from "../../generated/Role";
import {
  MAX_ANNOUNCEMENT_CHARS,
  announcementProblem,
  editAnnouncement,
  may,
  mayLeave as roleMayLeave,
  mayRemove as roleMayRemove,
} from "../../stores/groups";

/**
 * The Group info panel: Participants, their Roles, the announcement, and the
 * actions the caller's own Role permits.
 *
 * # The UI mirrors the server, it does not decide
 *
 * `jiuyue-chat::permission` is the one authority on who may do what. Every
 * predicate below delegates to `../../stores/groups`, which mirrors that decision
 * table in one place, so the panel cannot drift into a second, wrong, permission
 * model. They are **not** the enforcement — the server re-checks every action,
 * and a panel that showed a forbidden button would still be refused.
 */
const props = defineProps<{
  conversation: ConversationSummary;
  members: readonly MemberView[];
  currentUserId: string;
  loading: boolean;
}>();

const emit = defineEmits<{
  invite: [usernames: string[]];
  remove: [userId: string];
  setRole: [userId: string, role: Exclude<Role, "owner">];
  transfer: [userId: string];
  leave: [];
  dissolve: [];
}>();

const inviteInput = ref("");

const myRole = computed<Role>(
  () => props.conversation.group?.my_role ?? "member",
);

/** The group's announcement, or `null` when it has none. */
const announcement = computed<string | null>(
  () => props.conversation.group?.announcement ?? null,
);

/** Whether the announcement editor is open. */
const editingAnnouncement = ref(false);
/** The draft the editor holds while it is open. */
const announcementDraft = ref("");
/** The last edit failure, from the server or the local length mirror. */
const announcementError = ref<string | null>(null);
/** Whether a save is in flight. */
const savingAnnouncement = ref(false);

/** The draft's length in Unicode code points, for the live counter. */
const announcementLength = computed(() => [...announcementDraft.value].length);
/** Whether the draft is over the cap, so saving is refused before a round trip. */
const announcementTooLong = computed(
  () => announcementProblem(announcementDraft.value) !== null,
);

/**
 * The problem to show under the editor: the local length mirror first, then the
 * last server failure. `null` means there is nothing to report.
 */
const announcementMessage = computed<string | null>(
  () => announcementProblem(announcementDraft.value) ?? announcementError.value,
);

/** Owner or admin may invite (mirrors `Capability::InviteMembers`). */
const mayInvite = computed(() => may(myRole.value, "inviteMembers"));

/** Owner or admin may edit the group's own profile — title and announcement. */
const mayEditAnnouncement = computed(() => may(myRole.value, "editGroupInfo"));

/** Only the owner may dissolve (mirrors the owner-only rows). */
const mayDissolve = computed(() => may(myRole.value, "dissolve"));

/**
 * An owner with others present cannot leave; they must transfer or dissolve
 * first (mirrors `may_leave`). The panel says so instead of offering a refusal.
 */
const mayLeave = computed(() =>
  roleMayLeave(myRole.value, props.members.length - 1),
);

/** Whether the caller may remove `member` (mirrors `may_remove`). */
function mayRemove(member: MemberView): boolean {
  if (member.user_id === props.currentUserId) return false;
  return roleMayRemove(myRole.value, member.role);
}

/** Only the owner changes Roles, and never their own (mirrors `ChangeRoles`). */
function mayChangeRole(member: MemberView): boolean {
  return may(myRole.value, "changeRoles") && member.role !== "owner";
}

/** Only the owner transfers, and only to someone else. */
function mayTransfer(member: MemberView): boolean {
  return (
    may(myRole.value, "transferOwnership") &&
    member.user_id !== props.currentUserId
  );
}

const ROLE_LABELS: Record<Role, string> = {
  owner: "群主",
  admin: "管理员",
  member: "成员",
};

function displayName(member: MemberView): string {
  return member.display_name === "" ? member.username : member.display_name;
}

function invite(): void {
  const usernames = inviteInput.value
    .split(/[\s,，、]+/)
    .map((username) => username.trim())
    .filter((username) => username !== "");
  if (usernames.length === 0) return;

  emit("invite", usernames);
  inviteInput.value = "";
}

/** Open the editor with the stored announcement as its starting draft. */
function startEditingAnnouncement(): void {
  announcementDraft.value = announcement.value ?? "";
  announcementError.value = null;
  editingAnnouncement.value = true;
}

/** Abandon the draft; the stored announcement is unchanged. */
function cancelAnnouncement(): void {
  editingAnnouncement.value = false;
  announcementError.value = null;
}

/**
 * Save the draft through the shared announcement flow.
 *
 * The flow owns the server call and the length mirror; the panel only reports
 * what came back, so an over-long draft and a permission refusal are both
 * visible without the panel deciding either.
 */
async function saveAnnouncement(): Promise<void> {
  if (savingAnnouncement.value) return;

  const problem = announcementProblem(announcementDraft.value);
  if (problem !== null) {
    announcementError.value = problem;
    return;
  }

  savingAnnouncement.value = true;
  announcementError.value = null;
  const result = await editAnnouncement(
    props.conversation.id,
    announcementDraft.value,
  );
  savingAnnouncement.value = false;

  if (!result.ok) {
    announcementError.value = result.message;
    return;
  }
  editingAnnouncement.value = false;
}
</script>

<template>
  <section
    class="flex min-h-0 flex-1 flex-col overflow-y-auto"
    aria-label="群资料"
    data-test="group-info"
  >
    <header class="border-b border-zinc-200 px-4 py-3 dark:border-zinc-800">
      <h3 class="text-sm font-semibold" data-test="group-info-title">
        {{ conversation.group?.title ?? "群聊" }}
      </h3>
      <p class="text-xs text-zinc-500">
        {{ conversation.group?.member_count ?? members.length }} 位成员 ·
        我的角色：{{ ROLE_LABELS[myRole] }}
      </p>
    </header>

    <section
      class="border-b border-zinc-200 px-4 py-3 dark:border-zinc-800"
      data-test="announcement"
    >
      <div class="flex items-center justify-between gap-2">
        <h4 class="text-xs font-semibold text-zinc-500">群公告</h4>
        <button
          v-if="mayEditAnnouncement && !editingAnnouncement"
          type="button"
          class="rounded border border-zinc-300 px-2 py-1 text-xs transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
          data-test="announcement-edit"
          @click="startEditingAnnouncement"
        >
          编辑
        </button>
      </div>

      <template v-if="editingAnnouncement">
        <textarea
          v-model="announcementDraft"
          rows="4"
          placeholder="输入群公告，留空则清除"
          class="mt-2 w-full resize-y rounded-lg border border-zinc-300 bg-white px-3 py-2 text-sm outline-none transition-colors focus:border-zinc-500 dark:border-zinc-700 dark:bg-zinc-950"
          data-test="announcement-input"
        />
        <p
          class="mt-1 text-right text-xs text-zinc-400"
          data-test="announcement-count"
        >
          {{ announcementLength }} / {{ MAX_ANNOUNCEMENT_CHARS }}
        </p>
        <p
          v-if="announcementMessage !== null"
          class="mt-1 text-xs text-red-600 dark:text-red-400"
          data-test="announcement-error"
        >
          {{ announcementMessage }}
        </p>
        <div class="mt-2 flex gap-2">
          <button
            type="button"
            class="rounded-lg bg-zinc-900 px-3 py-1.5 text-xs font-medium text-white transition-colors hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-300"
            :disabled="savingAnnouncement || announcementTooLong"
            data-test="announcement-save"
            @click="saveAnnouncement"
          >
            保存
          </button>
          <button
            type="button"
            class="rounded-lg border border-zinc-300 px-3 py-1.5 text-xs transition-colors hover:bg-zinc-100 disabled:cursor-not-allowed disabled:opacity-50 dark:border-zinc-700 dark:hover:bg-zinc-800"
            :disabled="savingAnnouncement"
            data-test="announcement-cancel"
            @click="cancelAnnouncement"
          >
            取消
          </button>
        </div>
      </template>

      <p
        v-else-if="announcement !== null"
        class="mt-2 whitespace-pre-wrap break-words text-sm"
        data-test="announcement-text"
      >
        {{ announcement }}
      </p>
      <p
        v-else
        class="mt-2 text-sm text-zinc-400"
        data-test="announcement-empty"
      >
        暂无群公告
      </p>
    </section>

    <p v-if="loading" class="p-4 text-sm text-zinc-500">加载中…</p>

    <form
      v-if="mayInvite"
      class="flex gap-2 border-b border-zinc-200 p-3 dark:border-zinc-800"
      data-test="invite-form"
      @submit.prevent="invite"
    >
      <input
        v-model="inviteInput"
        type="text"
        placeholder="邀请用户名，逗号分隔"
        class="min-w-0 flex-1 rounded-lg border border-zinc-300 bg-white px-3 py-2 text-sm outline-none transition-colors focus:border-zinc-500 dark:border-zinc-700 dark:bg-zinc-950"
        data-test="invite-input"
      />
      <button
        type="submit"
        class="rounded-lg bg-zinc-900 px-3 py-2 text-sm font-medium text-white transition-colors hover:bg-zinc-700 disabled:cursor-not-allowed disabled:opacity-50 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-300"
        :disabled="inviteInput.trim() === ''"
        data-test="invite-submit"
      >
        邀请
      </button>
    </form>

    <ul role="list" class="divide-y divide-zinc-100 dark:divide-zinc-800">
      <li
        v-for="member in members"
        :key="member.user_id"
        class="flex items-center gap-3 px-4 py-3"
        data-test="member-row"
      >
        <span
          class="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-zinc-200 text-xs font-medium text-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
        >
          {{ displayName(member).slice(0, 1).toUpperCase() }}
        </span>
        <span class="min-w-0 flex-1">
          <span class="block truncate text-sm">
            {{ displayName(member) }}
            <span
              v-if="member.user_id === currentUserId"
              class="text-xs text-zinc-400"
              >（我）</span
            >
          </span>
          <span class="block truncate text-xs text-zinc-500">
            @{{ member.username }}
          </span>
        </span>

        <span
          class="shrink-0 rounded-full border border-zinc-300 px-2 py-0.5 text-xs dark:border-zinc-700"
          data-test="role-badge"
        >
          {{ ROLE_LABELS[member.role] }}
        </span>

        <span class="flex shrink-0 gap-1">
          <button
            v-if="mayChangeRole(member)"
            type="button"
            class="rounded border border-zinc-300 px-2 py-1 text-xs transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
            data-test="toggle-admin"
            @click="
              emit(
                'setRole',
                member.user_id,
                member.role === 'admin' ? 'member' : 'admin',
              )
            "
          >
            {{ member.role === "admin" ? "取消管理员" : "设为管理员" }}
          </button>
          <button
            v-if="mayTransfer(member)"
            type="button"
            class="rounded border border-zinc-300 px-2 py-1 text-xs transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
            data-test="transfer-owner"
            @click="emit('transfer', member.user_id)"
          >
            转让群主
          </button>
          <button
            v-if="mayRemove(member)"
            type="button"
            class="rounded border border-red-300 px-2 py-1 text-xs text-red-700 transition-colors hover:bg-red-50 dark:border-red-900 dark:text-red-400 dark:hover:bg-red-950/40"
            data-test="remove-member"
            @click="emit('remove', member.user_id)"
          >
            移出
          </button>
        </span>
      </li>
    </ul>

    <footer
      class="mt-auto space-y-2 border-t border-zinc-200 p-3 dark:border-zinc-800"
    >
      <button
        v-if="mayLeave"
        type="button"
        class="w-full rounded-lg border border-zinc-300 px-3 py-2 text-sm transition-colors hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
        data-test="leave-group"
        @click="emit('leave')"
      >
        退出群聊
      </button>
      <p v-else class="text-xs text-zinc-500" data-test="owner-leave-hint">
        群主需先转让群主或解散群聊，才能退出。
      </p>
      <button
        v-if="mayDissolve"
        type="button"
        class="w-full rounded-lg border border-red-300 px-3 py-2 text-sm text-red-700 transition-colors hover:bg-red-50 dark:border-red-900 dark:text-red-400 dark:hover:bg-red-950/40"
        data-test="dissolve-group"
        @click="emit('dissolve')"
      >
        解散群聊
      </button>
    </footer>
  </section>
</template>
