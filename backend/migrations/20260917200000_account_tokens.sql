-- Migration 20260917190000 — `account_tokens`
--
-- One table for the two one-shot links the identity module emails: the address
-- verification link a new account receives, and the password-reset link a user
-- requests when they are locked out. Both are the same shape of secret — a
-- random bearer value that expires and may be spent once — so they share a table
-- and are told apart by `purpose`.
--
-- Why a digest and not the token:
--   The value that travels in the email is 256 bits of OS randomness; only its
--   SHA-256 digest is stored here. A database leak therefore yields nothing a
--   client could present, exactly like `sessions.refresh_token_hash`. The CHECK
--   below pins the storage shape so a raw base64url token can never be written
--   into the column by mistake (43 base64url chars do not match; 64 lowercase
--   hex chars do).
--
-- Why `consumed_at` is an UPDATE, not a DELETE:
--   Redemption marks the row consumed in its own committed statement *before*
--   the caller does anything else, so a token that is being redeemed cannot be
--   replayed even if the step that follows it fails halfway. The row stays for
--   the audit trail and so "already used" is distinguishable from "never
--   existed".
--
-- Conventions inherited from `20260917120000_users.sql`:
--   * id       — application-generated ULID, CHAR(26), CHECK-pinned.
--   * timestamps — `timestamptz`, defaulted by PostgreSQL where sensible.
--   * secrets  — only ever stored hashed.

CREATE TABLE account_tokens (
    id          CHAR(26)    PRIMARY KEY,
    user_id     CHAR(26)    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Which one-shot link this is. A verification token cannot spend a reset and
    -- vice versa, because every query filters on the purpose.
    purpose     TEXT        NOT NULL,
    -- SHA-256 of the token that was emailed, hex-encoded. The token itself is
    -- returned exactly once (inside the link) and is unrecoverable from here.
    token_hash  TEXT        NOT NULL,
    expires_at  TIMESTAMPTZ NOT NULL,
    -- Non-NULL means already spent. A spent token is refused even before its
    -- `expires_at`, which is what makes single use real rather than advisory.
    consumed_at TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- ULID: 26 chars of Crockford Base32 (I, L, O, U excluded; case insensitive).
    CONSTRAINT account_tokens_id_ulid
        CHECK (id ~ '^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$'),
    -- The closed set of purposes, so a typo cannot create an unredeemable token.
    CONSTRAINT account_tokens_purpose
        CHECK (purpose IN ('email_verification', 'password_reset')),
    -- A 64-char lowercase hex digest, never the raw token.
    CONSTRAINT account_tokens_token_hash_format
        CHECK (token_hash ~ '^[0-9a-f]{64}$')
);

-- Redemption is a single equality probe on the digest, and the UNIQUE index
-- makes it impossible for two rows to answer to the same token.
CREATE UNIQUE INDEX account_tokens_token_hash_key ON account_tokens (token_hash);

-- Listing or invalidating a user's outstanding links of one purpose is the
-- common write (a new request retires the previous link), so it gets an index.
CREATE INDEX account_tokens_user_purpose_idx ON account_tokens (user_id, purpose);

-- Deleting a `users` row cascades here; PostgreSQL does not index FK columns
-- automatically, so the cascade gets its own index.
CREATE INDEX account_tokens_user_id_idx ON account_tokens (user_id);

COMMENT ON TABLE account_tokens IS
    'One-shot email links (address verification, password reset), stored as digests';
COMMENT ON COLUMN account_tokens.id IS
    'Application-generated ULID, CHAR(26)';
COMMENT ON COLUMN account_tokens.user_id IS
    'Owning User; ON DELETE CASCADE removes every outstanding link with the account';
COMMENT ON COLUMN account_tokens.purpose IS
    'email_verification | password_reset; a token only redeems for its own purpose';
COMMENT ON COLUMN account_tokens.token_hash IS
    'SHA-256 hex of the emailed token; the raw token is never stored';
COMMENT ON COLUMN account_tokens.expires_at IS
    'When the link stops being accepted';
COMMENT ON COLUMN account_tokens.consumed_at IS
    'When the link was redeemed; NULL means still unused (and redeemable once)';
