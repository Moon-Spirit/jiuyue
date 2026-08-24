-- M3: secret-chat server support — E2EE identity/key-bundle storage plus the
-- `secret` conversation pair-uniqueness index.
--
-- The server is a RELAY for secret chats: identity keys and one-time keys are
-- opaque PUBLIC material stored verbatim; private keys never touch this
-- schema. `one_time_keys` is a jsonb ARRAY of base64 strings so a bundle
-- fetch can atomically pop exactly one element (`jsonb - idx`) inside a
-- transaction (Signal-style consume-on-fetch, see crates/server/src/e2ee.rs).

CREATE TABLE e2ee_identities (
    id            uuid PRIMARY KEY,
    -- One bundle per device; re-uploads UPSERT onto the same row.
    device_id     uuid NOT NULL UNIQUE REFERENCES devices(id) ON DELETE CASCADE,
    user_id       uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    identity_key  text NOT NULL,
    one_time_keys jsonb NOT NULL DEFAULT '[]',
    uploaded_at   timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX idx_e2ee_identities_user ON e2ee_identities(user_id);

-- Secret direct conversations mirror the direct-pair create-or-get contract:
-- at most one row per unordered user pair among kind='secret' conversations
-- (same sorted pair_key logic as the partial index below it).
CREATE UNIQUE INDEX conversations_secret_pair_key_uq ON conversations(pair_key)
    WHERE kind = 'secret';
