-- Migration 20260917160000 — read state on `conversation_members`
--
-- The Read Marker and the Read Receipt of CONTEXT.md §已读与投递, plus the
-- denormalised Unread Count that renders the Conversation list.
--
-- # Two concepts, two columns
--
-- CONTEXT.md is explicit that a **Read Marker** (a User's *private* position:
-- "I have read up to here") and a **Read Receipt** (a Participant's *public*
-- acknowledgement, shown to the other Participants) are different concepts and
-- must never be conflated. They therefore get two separate columns:
--
--   * `read_marker_seq`  — PRIVATE.  Drives the unread count. Scoped to a User
--                          (this row is `(conversation, user)`, so it is one
--                          value shared by every Device of that User). Never
--                          returned to another Participant by any query: the
--                          peer-facing read model selects only `read_receipt_seq`.
--   * `read_receipt_seq` — PUBLIC.   The position broadcast to the other
--                          Participants. Selecting this column is the only way
--                          to build a receipt, so the private marker cannot be
--                          served as one.
--
-- Both are positions on the Conversation's Sequence Number (ADR-0003), exactly
-- like `sync_cursors.last_seq` — but at a different granularity: that cursor is
-- **per Device** (what a Device has *received*; ADR-0014), while this is **per
-- User** (what the User has *read*). They are different axes and are not merged.
--
-- # Why the Unread Count is a column, not a COUNT(*)
--
-- The Conversation list is the hottest read path, and it must not scan
-- `messages` once per Conversation per request. The count is therefore
-- maintained on the participant row:
--
--   * the send path increments it for every recipient, guarded by
--     `read_marker_seq < <the new seq>` so a Message the User has already read
--     can never re-inflate it, and in the same transaction that wrote the
--     Message, so a crash cannot leave the count and the stream disagreeing;
--   * the read path recomputes it from the same invariant when the marker
--     advances (a `count(*)` over the unread range of one Conversation, at human
--     frequency, is cheap and self-correcting).
--
-- Under concurrent sends the increments serialise on the participant row
-- (`UPDATE ... SET unread_count = unread_count + 1`), so N concurrent Messages
-- land on exactly N — not on a lost update. A replayed send is a no-op: the
-- increment lives inside the same transaction as the idempotent insert, which
-- rolls back on a replay.
--
-- # 0 is a valid position
--
-- Unlike `sync_cursors` (where 0 is reserved for "no row"), a membership row
-- always exists, so 0 legitimately means "has read nothing yet". The CHECKs
-- therefore only forbid negatives.

ALTER TABLE conversation_members
    ADD COLUMN read_marker_seq  BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN read_receipt_seq BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN unread_count     BIGINT NOT NULL DEFAULT 0;

ALTER TABLE conversation_members
    ADD CONSTRAINT conversation_members_read_marker_non_negative
        CHECK (read_marker_seq >= 0),
    ADD CONSTRAINT conversation_members_read_receipt_non_negative
        CHECK (read_receipt_seq >= 0),
    ADD CONSTRAINT conversation_members_unread_count_non_negative
        CHECK (unread_count >= 0);

COMMENT ON COLUMN conversation_members.read_marker_seq IS
    'PRIVATE per-User Read Marker (CONTEXT.md): highest Sequence Number the User has read. Drives unread_count; never shown to another Participant';
COMMENT ON COLUMN conversation_members.read_receipt_seq IS
    'PUBLIC per-Participant Read Receipt (CONTEXT.md): highest Sequence Number acknowledged to the other Participants';
COMMENT ON COLUMN conversation_members.unread_count IS
    'Denormalised Unread Count: Messages after read_marker_seq, excluding the User''s own; incremented with the Message insert, recomputed on read';
