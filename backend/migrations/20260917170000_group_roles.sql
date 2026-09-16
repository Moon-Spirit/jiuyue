-- Migration 20260917170000 — group titles and Participant Roles
--
-- The Group Conversation half of `conversations` / `conversation_members`
-- (ticket #14). It adds exactly two things to the schema the Direct path already
-- uses (migration 20260917140000):
--
--   * `conversations.title` — the group's name (CONTEXT.md: Group Conversation).
--     A Group always has one; a Direct Conversation never does. That equivalence
--     is pinned as a CHECK, so neither half can drift.
--
--   * `conversation_members.role` — the Participant's Role (CONTEXT.md: Role:
--     owner / admin / member). It is only meaningful in a Group; a Participant of
--     a Direct Conversation carries the default `member` and no permission rule
--     ever reads it.
--
-- Why a partial unique index for the owner: "exactly one owner" is an invariant
-- the permission model depends on (only the owner may transfer or dissolve), and
-- the cheapest place to guarantee it is the database. A partial UNIQUE index on
-- `conversation_id WHERE role = 'owner'` makes a second owner impossible; the
-- transfer path demotes before it promotes so the index is never transiently
-- violated. The index does not require a group to *have* an owner — the service
-- owns that half, because a group is created and populated in one transaction.
--
-- Every row of `conversations` and `conversation_members` belongs to the
-- `jiuyue-chat` crate (ADR-0011); nothing else reads or writes these columns.

-- The group's display name. NULL for a Direct Conversation, required for a Group.
ALTER TABLE conversations
    ADD COLUMN title TEXT;

-- A Group always has a title and a Direct Conversation never does. This is an
-- equality of booleans on purpose: it forbids both "group without a name" and
-- "direct with one".
ALTER TABLE conversations
    ADD CONSTRAINT conversations_title_required
        CHECK ((kind = 'group') = (title IS NOT NULL));

-- 1–100 characters: long enough for a real group name, short enough that a
-- hostile client cannot force a huge row. The chat crate rejects the same values
-- before the database sees them.
ALTER TABLE conversations
    ADD CONSTRAINT conversations_title_length
        CHECK (title IS NULL OR char_length(title) BETWEEN 1 AND 100);

COMMENT ON COLUMN conversations.title IS
    'Group display name (CONTEXT.md: Group Conversation); NULL for direct. Required for group';

-- The Participant's Role in a Group Conversation. `member` is the default so the
-- existing Direct rows are valid without a rewrite, and so `INSERT_MEMBERS` (the
-- Direct path) needs no change.
ALTER TABLE conversation_members
    ADD COLUMN role TEXT NOT NULL DEFAULT 'member';

ALTER TABLE conversation_members
    ADD CONSTRAINT conversation_members_role
        CHECK (role IN ('owner', 'admin', 'member'));

-- At most one owner per Conversation. The transfer path demotes the outgoing
-- owner before promoting the incoming one, so the index holds at every instant.
CREATE UNIQUE INDEX conversation_members_single_owner
    ON conversation_members (conversation_id)
    WHERE role = 'owner';

COMMENT ON COLUMN conversation_members.role IS
    'Role (CONTEXT.md: Role): owner | admin | member. Meaningful only in a Group; direct members are member';
