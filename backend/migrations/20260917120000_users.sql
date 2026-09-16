-- Migration 20260917120000 — `users`
--
-- User is the root of the identity aggregate (CONTEXT.md §身份与账号). This table
-- holds identity and profile only. Devices, login sessions, contacts and blocks
-- get their own tables in later migrations; nothing speculative is pre-created here.
--
-- Conventions established by this first schema, to be followed by later ones:
--   * id      — application-generated ULID (ADR-0003), stored as CHAR(26). The
--               CHECK pins the storage shape (26 chars of Crockford Base32, case
--               insensitive per the ULID spec); time-sortability is a property of
--               generation, not of storage, so it cannot be pinned by a constraint.
--   * username / email — both stored normalised to lowercase, so the UNIQUE
--               constraints are case-insensitive without citext or functional
--               indexes, and lookups are plain equality.
--   * timestamps — always `timestamptz`, always defaulted by PostgreSQL so the
--               application clock is never the source of truth.
--
-- Partitioning: `users` is the global identity list keyed by ULID; it is not
-- partitioned and reserves no partition key. The table that will need a partition
-- key is `messages` (by conversation_id, ADR-0005), not this one.

CREATE TABLE users (
    id                CHAR(26)    PRIMARY KEY,
    username          TEXT        NOT NULL UNIQUE,
    email             TEXT        NOT NULL UNIQUE,
    password_hash     TEXT,
    display_name      TEXT        NOT NULL,
    avatar_url        TEXT,
    email_verified_at TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- ULID: 26 chars of Crockford Base32 (I, L, O, U excluded; case insensitive).
    CONSTRAINT users_id_ulid
        CHECK (id ~ '^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$'),
    -- @handle: lowercase letters, digits and underscore, 3–32 chars.
    CONSTRAINT users_username_format
        CHECK (username ~ '^[a-z0-9_]{3,32}$'),
    -- Stored lowercase by construction; this makes the invariant explicit and
    -- keeps the UNIQUE constraint above case-insensitive.
    CONSTRAINT users_email_lowercase
        CHECK (email = lower(email)),
    CONSTRAINT users_display_name_length
        CHECK (char_length(display_name) BETWEEN 1 AND 64)
);

-- The UNIQUE constraints on username and email already create their own indexes,
-- so no additional index is needed for login/lookup by either column. Listing and
-- counting users by registration time (admin console, signup statistics) has no
-- unique constraint to lean on, so it gets an explicit index.
CREATE INDEX users_created_at_idx ON users (created_at);

COMMENT ON TABLE users IS 'User identity and profile (CONTEXT.md: User)';
COMMENT ON COLUMN users.id IS 'Application-generated ULID, CHAR(26), globally unique and time-sortable';
COMMENT ON COLUMN users.username IS 'Unique @handle, normalised lowercase, [a-z0-9_]{3,32}';
COMMENT ON COLUMN users.email IS 'Unique email, stored lowercase; OAuth-only accounts also carry one';
COMMENT ON COLUMN users.password_hash IS 'Password hash; NULL for OAuth-only accounts (no password login path)';
COMMENT ON COLUMN users.email_verified_at IS 'When the email was verified; NULL means not yet verified';
