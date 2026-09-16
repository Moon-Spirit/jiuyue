-- Migration 20260917140000 — `conversations`, `conversation_members`, `messages`
--
-- The Conversation aggregate (CONTEXT.md §会话) and its Message stream
-- (CONTEXT.md §消息). These three tables belong to the `jiuyue-chat` crate and to
-- nobody else (ADR-0011): any other domain that needs them asks that crate for a
-- method, it does not read the tables.
--
-- Conventions inherited from `20260917120000_users.sql`:
--   * id         — application-generated ULID (ADR-0003), stored as CHAR(26). The
--                  CHECK pins the storage shape, not the (time-sortable) generation.
--   * timestamps — always `timestamptz`, always defaulted by PostgreSQL so the
--                  application clock is never the source of truth.
--
-- Shape rules the delivery protocol (ADR-0003) depends on:
--
--   * A Direct Conversation is unique per unordered pair of Users. The canonical
--     `direct_key` — the two ULIDs joined in sorted order — carries a UNIQUE
--     constraint, so two Users opening the same chat at the same instant converge
--     on one row through `INSERT ... ON CONFLICT` (a race the application cannot
--     lose). A check-then-insert would create a second row; this cannot.
--
--   * `messages.seq` is the per-Conversation ordering authority. It is allocated
--     atomically out of `conversations.next_seq`
--     (`UPDATE conversations SET next_seq = next_seq + 1 WHERE id = $1 RETURNING`).
--     Read-then-write would hand two concurrent sends the same number and break
--     ordering for good.
--
--   * `(sender_id, client_msg_id)` is UNIQUE: `client_msg_id` is the sender's
--     idempotency key (CONTEXT.md: Client Message ID), so replaying a send whose
--     acknowledgement was lost returns the original row instead of writing a
--     second Message. This is what lets the client retry without duplicating.
--
-- Partitioning: `messages` is the table ADR-0005 anticipates partitioning by
-- `conversation_id`. Partitioning is deliberately not enabled now — but every
-- UNIQUE constraint on this table includes `conversation_id`, so the partition
-- key a later migration picks needs no index rewrite. (`messages` sorts by
-- `(conversation_id, seq)`, never by a global sequence.)

CREATE TABLE conversations (
    id         CHAR(26)    PRIMARY KEY,
    kind       TEXT        NOT NULL,
    direct_key TEXT,
    -- The next Sequence Number to hand out. Starts at 0 so the first allocation
    -- returns 1; the UPDATE in the send path is the only writer.
    next_seq   BIGINT      NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- ULID: 26 chars of Crockford Base32 (I, L, O, U excluded; case insensitive).
    CONSTRAINT conversations_id_ulid
        CHECK (id ~ '^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$'),
    CONSTRAINT conversations_kind
        CHECK (kind IN ('direct', 'group')),
    -- A Direct Conversation always has a pair key; a Group never does. This is an
    -- equality of booleans on purpose: it forbids both "direct without a key" and
    -- "group with one".
    CONSTRAINT conversations_direct_key_required
        CHECK ((kind = 'direct') = (direct_key IS NOT NULL)),
    -- Canonical "<lo>:<hi>": both participants compute the same key for the same
    -- pair, and the CHECK refuses a key stored in the other order, so the UNIQUE
    -- constraint below really is "one row per pair" rather than "one per ordering".
    CONSTRAINT conversations_direct_key_canonical
        CHECK (
            direct_key IS NULL
            OR (
                direct_key ~ '^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}:[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$'
                AND split_part(direct_key, ':', 1) < split_part(direct_key, ':', 2)
            )
        ),
    CONSTRAINT conversations_next_seq_non_negative
        CHECK (next_seq >= 0)
);

-- "One Direct Conversation per unordered pair" is this constraint. NULLs are
-- distinct in PostgreSQL's UNIQUE, so the group rows (direct_key IS NULL) never
-- collide with each other.
ALTER TABLE conversations
    ADD CONSTRAINT conversations_direct_key_key UNIQUE (direct_key);

CREATE TABLE conversation_members (
    conversation_id CHAR(26)    NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    user_id         CHAR(26)    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    joined_at       TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (conversation_id, user_id),
    CONSTRAINT conversation_members_conversation_id_ulid
        CHECK (conversation_id ~ '^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$'),
    CONSTRAINT conversation_members_user_id_ulid
        CHECK (user_id ~ '^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$')
);

-- Listing a User's Conversations filters by `user_id`; PostgreSQL does not index
-- FK columns automatically, and the primary key's leading column is the wrong one
-- for this access path.
CREATE INDEX conversation_members_user_id_idx ON conversation_members (user_id);

CREATE TABLE messages (
    id              CHAR(26)    PRIMARY KEY,
    -- The partition key ADR-0005 reserves. Present from the first row so a later
    -- `PARTITION BY` migration is a data move, not a redesign.
    conversation_id CHAR(26)    NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    -- Per-Conversation ordering authority; allocated atomically by the chat crate.
    seq             BIGINT      NOT NULL,
    sender_id       CHAR(26)    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- The sender's idempotency key (CONTEXT.md: Client Message ID). Opaque to the
    -- server; only uniqueness matters here.
    client_msg_id   TEXT        NOT NULL,
    body            TEXT        NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Two Messages can never share a slot in a Conversation. The index this
    -- constraint creates is also the one history reads use.
    CONSTRAINT messages_conversation_seq_key UNIQUE (conversation_id, seq),
    -- Replaying a send cannot write a second row. This is the retry guarantee.
    CONSTRAINT messages_sender_client_msg_key UNIQUE (sender_id, client_msg_id),
    CONSTRAINT messages_id_ulid
        CHECK (id ~ '^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$'),
    CONSTRAINT messages_conversation_id_ulid
        CHECK (conversation_id ~ '^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$'),
    CONSTRAINT messages_sender_id_ulid
        CHECK (sender_id ~ '^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$'),
    -- seq starts at 1; 0 is the "no messages yet" value of next_seq, never a Message.
    CONSTRAINT messages_seq_positive
        CHECK (seq > 0),
    CONSTRAINT messages_client_msg_id_length
        CHECK (char_length(client_msg_id) BETWEEN 1 AND 128),
    -- 1–4000 characters. The upper bound keeps a hostile body from forcing an
    -- unbounded row; the chat crate rejects the same values before the database.
    CONSTRAINT messages_body_length
        CHECK (char_length(body) BETWEEN 1 AND 4000)
);

COMMENT ON TABLE conversations IS 'A Conversation (CONTEXT.md: Conversation); Direct or Group';
COMMENT ON COLUMN conversations.id IS 'Application-generated ULID, CHAR(26)';
COMMENT ON COLUMN conversations.kind IS 'direct | group; only direct exists until the group ticket';
COMMENT ON COLUMN conversations.direct_key IS 'Canonical "<lo-ulid>:<hi-ulid>" pair key for direct; NULL for group. UNIQUE';
COMMENT ON COLUMN conversations.next_seq IS 'Atomic Sequence Number allocator; the send path increments it, nothing else writes it';

COMMENT ON TABLE conversation_members IS 'Membership of a User in a Conversation (CONTEXT.md: Participant)';
COMMENT ON COLUMN conversation_members.conversation_id IS 'Owning Conversation; ON DELETE CASCADE removes the membership';
COMMENT ON COLUMN conversation_members.user_id IS 'Participating User; ON DELETE CASCADE removes the membership';
COMMENT ON COLUMN conversation_members.joined_at IS 'When the User became a Participant';

COMMENT ON TABLE messages IS 'A Message in a Conversation (CONTEXT.md: Message), ordered by seq';
COMMENT ON COLUMN messages.id IS 'Global Message ID: application-generated ULID, time-sortable';
COMMENT ON COLUMN messages.conversation_id IS 'Owning Conversation; the partition key ADR-0005 reserves';
COMMENT ON COLUMN messages.seq IS 'Per-Conversation Sequence Number; the ordering authority (ADR-0003)';
COMMENT ON COLUMN messages.sender_id IS 'Sending User; UNIQUE with client_msg_id for send idempotency';
COMMENT ON COLUMN messages.client_msg_id IS 'Sender-generated idempotency key (CONTEXT.md: Client Message ID)';
COMMENT ON COLUMN messages.body IS 'Message text, 1–4000 characters';
