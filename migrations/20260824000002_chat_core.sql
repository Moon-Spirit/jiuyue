-- Ticket 05: chat core schema — conversations, membership, messages.
--
-- DEVIATION NOTE (deliberate, reviewed): the ticket draft sketched
-- `conversations(id uuid pk)`, but the frozen wire protocol crate
-- (jiuyue-protocol) types `conversation_id` as `i64` in every frame
-- (`MsgSend`, `MsgNew`, `SyncCursor`) and integration tests must round-trip
-- real frames through those exact types. A uuid PK cannot be carried on the
-- v1 wire without breaking the protocol contract, so conversations use a
-- BIGINT identity PK that maps 1:1 onto the wire value. Message ids stay
-- UUIDv7 (app-side `Uuid::now_v7`), matching `MsgNew.message_id`.
--
-- UUIDv7 PKs elsewhere are generated app-side; conversation ids are the one
-- DB-allocated identity in the schema (allocated atomically with pair_key
-- create-or-get below).

CREATE TABLE conversations (
    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind       text NOT NULL CHECK (kind IN ('direct', 'secret', 'group')) DEFAULT 'direct',
    -- Direct conversations only: lexicographically sorted "uuidA:uuidB" of
    -- the two member ids. Backed by a partial unique index so create-or-get
    -- is race-safe under concurrent inserts (ON CONFLICT DO NOTHING).
    pair_key   text NULL,
    created_by uuid NULL REFERENCES users(id),
    last_seq   bigint NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- Race-safe direct-conversation create-or-get: at most one row per unordered
-- user pair among direct conversations.
CREATE UNIQUE INDEX conversations_direct_pair_key_uq ON conversations(pair_key)
    WHERE kind = 'direct';

CREATE TABLE conversation_members (
    conversation_id    bigint NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    user_id            uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role               text NOT NULL DEFAULT 'member' CHECK (role IN ('owner', 'member')),
    last_delivered_seq bigint NOT NULL DEFAULT 0,
    last_read_seq      bigint NOT NULL DEFAULT 0,
    joined_at          timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (conversation_id, user_id)
);

CREATE INDEX idx_conversation_members_user_id ON conversation_members(user_id);

CREATE TABLE messages (
    id              uuid PRIMARY KEY,
    conversation_id bigint NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    seq             bigint NOT NULL,
    sender_id       uuid NOT NULL REFERENCES users(id),
    client_msg_id   uuid NOT NULL,
    kind            text NOT NULL DEFAULT 'text',
    -- At-rest encryption key generation tag ("v1" = AES-256-GCM keyed by
    -- SHA-256(JIUYUE_MASTER_KEY); see crates/server/src/crypto.rs).
    key_id          text NOT NULL,
    -- nonce(12) || AES-256-GCM ciphertext of the plaintext body.
    body_enc        bytea NOT NULL,
    sent_at         timestamptz NOT NULL DEFAULT now(),
    UNIQUE (conversation_id, seq),
    UNIQUE (conversation_id, client_msg_id)
);

CREATE INDEX idx_messages_conversation_seq ON messages(conversation_id, seq);
