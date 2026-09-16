<script setup lang="ts">
import { computed, ref } from "vue";
import type { ConversationSummary } from "../../generated/ConversationSummary";
import type { MemberView } from "../../generated/MemberView";
import type { Role } from "../../generated/Role";

/**
 * The Group info panel: Participants, their Roles, and the actions the caller's
 * own Role permits.
 *
 * # The UI mirrors the server, it does not decide
 *
 * `jiuyue-chat::permission` is the one authority on who may do what; the
 * predicate helpers below mirror that decision table so the panel does not offer
 * an action that will be refused. They are **not** the enforcement — the server
 * re-checks every action, and a panel that showed a forbidden button would still
 * be refused. Keeping the mirror small and in one file is what stops it drifting
 * into a second, wrong, permission model.
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
const isOwner = computed(() => myRole.value === "owner");
const isAdmin = computed(() => isOwner.value || myRole.value === "admin");

/** Owner or admin may invite (mirrors `Capability::InviteMembers`). */
const mayInvite = computed(() => isAdmin.value);

/** Only the owner may dissolve or transfer (mirrors the owner-only rows). */
const mayDissolve = computed(() => isOwner.value);

/**
 * An owner with others present cannot leave; they must transfer or dissolve
 * first (mirrors `may_leave`). The panel says so instead of offering a refusal.
 */
const mayLeave = computed(() => !isOwner.value || props.members.length <= 1);

/** Whether the caller may remove `member` (mirrors `may_remove`). */
function mayRemove(member: MemberView): boolean {
  if (member.user_id === props.currentUserId) return false;
  if (myRole.value === "owner") return member.role !== "owner";
  if (myRole.value === "admin") return member.role === "member";
  return false;
}

/** Only the owner changes Roles, and never their own (mirrors `ChangeRoles`). */
function mayChangeRole(member: MemberView): boolean {
  return isOwner.value && member.role !== "owner";
}

/** Only the owner transfers, and only to someone else. */
function mayTransfer(member: MemberView): boolean {
  return isOwner.value && member.user_id !== props.currentUserId;
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
