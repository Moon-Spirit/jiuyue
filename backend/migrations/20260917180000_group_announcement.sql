-- Migration 20260917180000 — the Group announcement
--
-- The Group Conversation's announcement (ticket #15): one bounded piece of text
-- that every Participant can read and only an owner or admin may edit
-- (`Capability::EditGroupInfo`).
--
-- Why a column on `conversations` and not its own table: an announcement is
-- exactly one optional value per Conversation — the same cardinality as the
-- group `title` added in 20260917170000, and it is read on the same rows. A
-- 1:1 table would add a join, a foreign key and a lifecycle for no invariant
-- it could enforce beyond the CHECKs below, and it would put the group's own
-- profile in two places.
--
-- What the schema guarantees (the service rejects the same values first):
--
--   * A Direct Conversation never carries one. That equivalence is a CHECK, so
--     neither half can drift — the same shape as `conversations_title_required`.
--   * An announcement, when present, is 1–2000 characters. The upper bound is
--     what keeps a hostile client from forcing an unbounded row on the 2 vCPU /
--     2 GB production box (ADR-0010). 2000 is deliberately below the 4000-char
--     Message cap: an announcement is a pinned notice, not a Message stream.
--   * "No announcement" is NULL, never the empty string; clearing an
--     announcement stores NULL. The service normalises to that shape.
--
-- Every row of `conversations` belongs to the `jiuyue-chat` crate (ADR-0011);
-- nothing else reads or writes this column.

-- Optional: a group may have no announcement, so no NOT NULL and no
-- "group must have one" constraint. The seeder in backup-restore.yml (which
-- writes the group without naming this column) keeps working unchanged.
ALTER TABLE conversations
    ADD COLUMN announcement TEXT;

-- A Group may have an announcement; a Direct Conversation never may. Stated as
-- an implication (`kind = 'group' OR announcement IS NULL`) rather than an
-- equality, because an absent announcement is a valid group state.
ALTER TABLE conversations
    ADD CONSTRAINT conversations_announcement_group_only
        CHECK (kind = 'group' OR announcement IS NULL);

-- 1–2000 characters when present. The chat crate rejects the same values before
-- the database sees them; this is the backstop that makes the bound structural.
ALTER TABLE conversations
    ADD CONSTRAINT conversations_announcement_length
        CHECK (announcement IS NULL OR char_length(announcement) BETWEEN 1 AND 2000);

COMMENT ON COLUMN conversations.announcement IS
    'Group announcement (CONTEXT.md: Group Conversation); NULL when absent or cleared. Required-kind: group only';
