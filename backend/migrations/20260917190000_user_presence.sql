-- Migration 20260917190000 — `user_presence`
--
-- One row per **User**: the instant they were last reachable (CONTEXT.md
-- §在线与状态: Presence). This is the only presence state that is durable; whether
-- a User is online right now is derived from their live Devices and lives in the
-- realtime process's connection registry, never in the database (ADR-0010 keeps
-- Redis out of the single-node phase, and a `last_seen` is all a restart needs).
--
-- Why the key is `user_id` and not a Device:
--   * Presence is per **User** (CONTEXT.md): a User is reachable if *any* of their
--     Devices is connected. The read model is "one reachability per person", so the
--     row is keyed by the person.
--   * A Device (`sessions`) already carries its own `last_seen_at` for the device
--     list, but that is a different fact ("this Device was last authenticated").
--     Folding them together would make a phone's activity speak for a laptop.
--
-- Why a table at all: online/offline live in process memory, so a restart makes
-- every User offline by construction — but "最后一次在线 3 分钟前" must survive that
-- restart. Only the timestamp is persisted.
--
-- Write discipline follows ADR-0014 (per-Device sync cursors): the realtime gateway
-- coalesces the Users to stamp in memory and writes them in **one batched
-- statement** per checkpoint — on the connection heartbeat, on a User's final
-- Device disconnecting, and when the pending set crosses a batch bound — never one
-- write per presence change. `now()` is PostgreSQL's, not the application's, so the
-- stored instant is the database's own clock (the schema convention from
-- `20260917120000_users.sql`).
--
-- Monotonic: the upsert takes `GREATEST(existing, incoming)`, so an out-of-order or
-- replayed checkpoint can never move `last_seen_at` backwards. A crash between two
-- checkpoints loses at most one heartbeat interval of precision: the Users who were
-- online are re-stamped on the next beat, and the ones who went offline keep the
-- last instant PostgreSQL recorded for them.

CREATE TABLE user_presence (
    user_id      CHAR(26)    PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    -- When this User was last reachable, stamped by the database clock.
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- ULID: 26 chars of Crockford Base32 (I, L, O, U excluded; case insensitive).
    CONSTRAINT user_presence_user_id_ulid
        CHECK (user_id ~ '^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$')
);

COMMENT ON TABLE user_presence IS 'When a User was last reachable (CONTEXT.md: Presence); online state itself is in-process, never stored';
COMMENT ON COLUMN user_presence.user_id IS 'The User (presence is per User, not per Device); ON DELETE CASCADE removes it with the account';
COMMENT ON COLUMN user_presence.last_seen_at IS 'Last instant the User was reachable; GREATEST on write, never rewound';
COMMENT ON COLUMN user_presence.updated_at IS 'When this row was last checkpointed, for diagnostics';
