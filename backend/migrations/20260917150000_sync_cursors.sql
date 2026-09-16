-- Migration 20260917150000 — `sync_cursors`
--
-- One row per (Device, Conversation): the position that Device has **consumed** in
-- that Conversation's Message stream (CONTEXT.md §已读与投递: Sync Cursor — 按 Device).
-- ADR-0013 kept the reconnect cursor in the client's memory and deferred the
-- server-side per-Device store to a later ticket; this is that ticket.
--
-- Why the pair is the key:
--   * A Device follows many Conversations, and a Conversation is followed by many
--     Devices. The cursor belongs to the intersection, so `(session_id,
--     conversation_id)` is the primary key — that is what makes "each Device
--     advances its own cursor independently" a schema fact rather than a promise.
--   * `session_id` references the `sessions` table, which already models a
--     logged-in Device (`device_label`, `user_agent`, `last_seen_at`, `revoked_at`).
--     Deleting a session (logout cascade, account delete) takes its cursors with it.
--
-- Granularity is the Conversation's Sequence Number (ADR-0003), the one ordering
-- authority. `last_seq` is a **safe promise**: nothing at or below it still needs
-- sending, so a client must never report a value with a hole below it. Everything
-- after it is what a returning Device repairs with the existing forward walk
-- (`GET /conversations/{id}/messages?after=`). No parallel cursor is invented —
-- storing anything other than `seq` would be a second source of truth.
--
-- Monotonic: the checkpoint statement takes `GREATEST(existing, incoming)`, so two
-- live connections of one Device reporting out of order, or a replayed report,
-- can never rewind a cursor. `last_seq` is 1-based like `messages.seq`; a cursor
-- at 0 would be indistinguishable from "no row", so zero is not storable.
--
-- Write amplification is bounded by the process, not by the client: the realtime
-- connection coalesces reports in memory (at most one entry per Conversation) and
-- flushes them on its heartbeat, on connection teardown, and when the pending set
-- crosses a batch bound. A process death between checkpoints loses at most the
-- tail of one heartbeat interval, which the Device re-fetches idempotently —
-- delivery is at-least-once and application is keyed on Message ID (ADR-0003).

CREATE TABLE sync_cursors (
    session_id      CHAR(26)    NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    conversation_id CHAR(26)    NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    -- The last consumed Sequence Number in this Conversation for this Device.
    last_seq        BIGINT      NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- A Device has exactly one position in a Conversation. ON CONFLICT on this key
    -- is what makes checkpointing idempotent.
    PRIMARY KEY (session_id, conversation_id),

    -- ULID: 26 chars of Crockford Base32 (I, L, O, U excluded; case insensitive).
    CONSTRAINT sync_cursors_session_id_ulid
        CHECK (session_id ~ '^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$'),
    CONSTRAINT sync_cursors_conversation_id_ulid
        CHECK (conversation_id ~ '^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$'),
    -- seq starts at 1 (see messages_seq_positive); a stored 0 would mean "no row".
    CONSTRAINT sync_cursors_last_seq_positive
        CHECK (last_seq >= 1)
);

-- Deleting a Conversation cascades here, and nothing else filters by
-- `conversation_id` alone; PostgreSQL does not index FK columns automatically and
-- the primary key's leading column is `session_id`, so the cascade needs this.
CREATE INDEX sync_cursors_conversation_id_idx ON sync_cursors (conversation_id);

COMMENT ON TABLE sync_cursors IS 'A Device''s consumed position in one Conversation (CONTEXT.md: Sync Cursor), per Device';
COMMENT ON COLUMN sync_cursors.session_id IS 'The Device (sessions row) that consumed; ON DELETE CASCADE removes its cursors';
COMMENT ON COLUMN sync_cursors.conversation_id IS 'The Conversation whose stream was consumed; ON DELETE CASCADE removes the cursor';
COMMENT ON COLUMN sync_cursors.last_seq IS 'Highest consumed Sequence Number (ADR-0003); GREATEST on write, never rewound';
COMMENT ON COLUMN sync_cursors.updated_at IS 'When this Device last advanced the cursor; used to bound the per-connection sync frame';
