-- Migration 20260917130000 — `sessions`
--
-- One row per logged-in Device (CONTEXT.md §身份与账号: Device). A Device is a
-- logged-in client instance, so this table — not `users` — is where "how many
-- places am I signed in" lives.
--
-- Why the access token is not self-sufficient: a signed JWT cannot be revoked, so
-- a stateless token would keep working until it expired. Instead the access token
-- carries this row's `id` in its `sid` claim and every authenticated request
-- re-checks the row (see `jiuyue-auth`). Logout sets `revoked_at`, and the very
-- next request sees it — revocation is immediate, not eventual.
--
-- Conventions inherited from `20260917120000_users.sql`:
--   * id            — application-generated ULID, stored as CHAR(26), CHECK-pinned.
--   * timestamps    — always `timestamptz`, defaulted by PostgreSQL.
--   * secrets       — only ever stored hashed (`refresh_token_hash`), never raw.
--
-- Shaped for the follow-on device ticket: the metadata columns (`device_label`,
-- `user_agent`, `ip_address`, `last_seen_at`) are exactly the fields a device
-- list renders, and `refresh_token_hash` is a plain unique column that a future
-- rotation simply UPDATEs in place. Nothing speculative is pre-created here.

CREATE TABLE sessions (
    id                 CHAR(26)    PRIMARY KEY,
    user_id            CHAR(26)    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- SHA-256 of the opaque refresh token, hex-encoded. The token itself is
    -- returned to the client exactly once and is unrecoverable from this column.
    refresh_token_hash TEXT        NOT NULL,
    -- Human label for the device list, filled in when the client tells us one.
    device_label       TEXT,
    user_agent         TEXT,
    ip_address         TEXT,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Refresh-token lifetime. An access token expires on its own (JWT `exp`).
    expires_at         TIMESTAMPTZ NOT NULL,
    -- Non-NULL means logged out or otherwise revoked; revocation wins over `expires_at`.
    revoked_at         TIMESTAMPTZ,

    -- ULID: 26 chars of Crockford Base32 (I, L, O, U excluded; case insensitive).
    CONSTRAINT sessions_id_ulid
        CHECK (id ~ '^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$'),
    -- Pins the storage shape so a raw token can never be written here by mistake:
    -- a 43-char base64url token does not match, a 64-char lowercase hex digest does.
    CONSTRAINT sessions_refresh_token_hash_format
        CHECK (refresh_token_hash ~ '^[0-9a-f]{64}$')
);

-- A refresh token maps to exactly one session, so the lookup at refresh time is a
-- single equality probe on a unique index — and a replayed token cannot match two rows.
CREATE UNIQUE INDEX sessions_refresh_token_hash_key ON sessions (refresh_token_hash);

-- Deleting a `users` row cascades here, so this index serves both that cascade and
-- the per-user device listing. PostgreSQL does not index FK columns automatically.
CREATE INDEX sessions_user_id_idx ON sessions (user_id);

COMMENT ON TABLE sessions IS 'A logged-in Device (CONTEXT.md: Device); one access-token `sid` per row';
COMMENT ON COLUMN sessions.id IS 'Application-generated ULID, CHAR(26); carried in the access token as `sid`';
COMMENT ON COLUMN sessions.user_id IS 'Owning User; ON DELETE CASCADE removes every session with the account';
COMMENT ON COLUMN sessions.refresh_token_hash IS 'SHA-256 hex of the opaque refresh token; the raw token is never stored';
COMMENT ON COLUMN sessions.device_label IS 'Client-supplied label for the device list (e.g. "Chrome on Windows")';
COMMENT ON COLUMN sessions.user_agent IS 'User-Agent header observed at login, for the device list';
COMMENT ON COLUMN sessions.ip_address IS 'Observed client address at login, for the device list';
COMMENT ON COLUMN sessions.last_seen_at IS 'Last time this session was refreshed or authenticated';
COMMENT ON COLUMN sessions.expires_at IS 'When the refresh token stops being accepted';
COMMENT ON COLUMN sessions.revoked_at IS 'When the session was revoked (logout); NULL means still active';
