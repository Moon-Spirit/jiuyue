-- Ticket 04: auth & device core schema.
-- UUIDv7 PKs are generated app-side (Uuid::now_v7), hence no DB defaults here.
-- Idempotency: sqlx's migrator records this file in _sqlx_migrations; re-running
-- `MIGRATOR.run` is a no-op once applied.

CREATE TABLE users (
    id            uuid PRIMARY KEY,
    username      text NOT NULL UNIQUE CHECK (length(username) BETWEEN 3 AND 32),
    display_name  text NOT NULL DEFAULT '',
    -- argon2id PHC string. Kept on users (not in auth_identities) because a
    -- password authenticates the account itself, not an external identity.
    password_hash text NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE auth_identities (
    id          uuid PRIMARY KEY,
    user_id     uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind        text NOT NULL CHECK (kind IN ('email', 'phone')),
    value       text NOT NULL,
    verified_at timestamptz,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (kind, value)
);

CREATE INDEX idx_auth_identities_user_id ON auth_identities(user_id);

CREATE TABLE devices (
    id           uuid PRIMARY KEY,
    user_id      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    platform     text NOT NULL DEFAULT 'web',
    push_token   text,
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE refresh_tokens (
    id         uuid PRIMARY KEY,
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash bytea NOT NULL UNIQUE,
    device_id  uuid REFERENCES devices(id),
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX idx_refresh_tokens_user_id ON refresh_tokens(user_id);
