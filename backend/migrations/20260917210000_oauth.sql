-- Migration 20260917210000 — third-party sign-in (`oauth_identities`, `oauth_states`,
-- `oauth_pending_sessions`)
--
-- Three tables, one for each thing that must survive a round trip to a provider
-- and back. Read together with ADR-0016 (third-party sign-in), which records why
-- the account-linking rule is what it is.
--
-- ---------------------------------------------------------------------------
-- `oauth_identities` — a provider credential belongs to exactly one account
-- ---------------------------------------------------------------------------
--
-- The provider's **subject** is the only stable identity. An email address is
-- not: a provider may let a user change it, may report an unverified one, and
-- two providers will happily report the same address for two different people.
-- Keying on `(provider, subject)` and never on email is what makes "sign in with
-- the same provider identity again" land on the same account, and it is also
-- what makes it impossible for a provider to *claim* an account by asserting an
-- address it happens to know. A matching email is therefore NOT a link; it is a
-- refusal telling the user to sign in their usual way and link from settings.
--
-- One account may hold several provider identities (GitHub and Google on the
-- same User); a provider identity belongs to exactly one account, which the
-- unique index below enforces rather than trusts.
--
-- No provider access/refresh tokens are stored. Nothing in this feature calls the
-- provider on the user's behalf after sign-in, so keeping them would only be a
-- liability: a second credential to leak that earns nothing. The exchange is
-- used to read the identity and then the tokens are dropped.
--
-- ---------------------------------------------------------------------------
-- `oauth_states` — the CSRF state and the PKCE verifier, single-use and short-lived
-- ---------------------------------------------------------------------------
--
-- `state` and the PKCE `code_verifier` are the two secrets the authorization
-- round trip needs. `state` is what stops a login-CSRF: without it an attacker
-- can hand a victim a crafted `?code=…` callback and sign the victim's browser
-- into the attacker's account (or, with the take-over bug this ticket exists to
-- avoid, into the victim's). The verifier is what stops an intercepted
-- authorization code from being redeemed by anyone but us.
--
-- Both are stored as digests, like `sessions.refresh_token_hash` and
-- `account_tokens.token_hash`: the raw `state` travels only in the browser's
-- authorization URL, and the raw verifier only to the provider's token endpoint.
-- A database leak therefore yields neither.
--
-- `expires_at` follows the `account_tokens` convention: the database clock
-- decides, `now() + make_interval(...)`, so issuance and redemption can never
-- disagree about whether a state is still alive.
--
-- `user_id` is nullable and means "this round trip is a *linking* attempt by an
-- already-authenticated user", not "sign in". NULL is the login flow, which is
-- the common case; a link attempt names the account up front.
--
-- ---------------------------------------------------------------------------
-- `oauth_pending_sessions` — the limited session, not a full one
-- ---------------------------------------------------------------------------
--
-- A first-time provider user has no username. That is a real state, and it must
-- be impossible to confuse with a full session: the value in this table is an
-- opaque 256-bit token, *not* a JWT. Every protected endpoint presents an
-- access token and nothing else can satisfy it, so the limited session is
-- refused everywhere by construction rather than by remembering to check a flag.
-- Only `POST /auth/oauth/complete` redeems it, and redemption spends it before
-- the username write, exactly like every other one-shot in `account_tokens`.
--
-- Keyed by `(provider, subject)` rather than by `user_id` on purpose: a user who
-- abandons the username step must be able to return. On the next callback the
-- provider identity still names the account, and a fresh limited token can be
-- minted for the *same* account instead of leaving a username-less user sealed
-- behind a lost token. A separate `consumed_at` lets a spent row be retired
-- while `(provider, subject)` stays unique.
--
-- Conventions inherited from `20260917120000_users.sql` and
-- `20260917200000_account_tokens.sql`:
--   * id       — application-generated ULID, CHAR(26), CHECK-pinned.
--   * secrets  — only ever stored hashed, CHECK-pinned to the digest shape.
--   * timestamps — `timestamptz`, defaulted by PostgreSQL; expiry by `now()`.

CREATE TABLE oauth_identities (
    id            CHAR(26)    PRIMARY KEY,
    user_id       CHAR(26)    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Which provider. A closed set named by the application, never free text, so
    -- a typo cannot create an identity no query will ever find.
    provider      TEXT        NOT NULL,
    -- The provider's own immutable id for the account ("sub" in OpenID Connect).
    -- This is the credential. The email is not.
    subject       TEXT        NOT NULL,
    -- What the provider called the account, for display only; may go stale.
    display_name  TEXT,
    -- The address the provider reported, if any, for the account-holder to read.
    -- Informational, never a matching key.
    email         TEXT,
    -- Whether the provider asserted the address is verified. Informational too:
    -- the linking rule does not consult it, because a verified address is still
    -- not proof that this provider identity owns *our* account.
    email_verified BOOLEAN    NOT NULL DEFAULT FALSE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT oauth_identities_id_ulid
        CHECK (id ~ '^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$'),
    CONSTRAINT oauth_identities_provider
        CHECK (provider IN ('github', 'google')),
    -- Provider subjects are opaque strings; bounded so a hostile provider
    -- response cannot write an unbounded value, never empty.
    CONSTRAINT oauth_identities_subject_length
        CHECK (char_length(subject) BETWEEN 1 AND 255)
);

-- The whole feature rests on this index: one provider identity maps to one
-- account, and "sign in again" is a single equality probe against it.
CREATE UNIQUE INDEX oauth_identities_provider_subject_key
    ON oauth_identities (provider, subject);

-- Listing an account's linked providers is the settings-page read.
CREATE INDEX oauth_identities_user_id_idx ON oauth_identities (user_id);

CREATE TABLE oauth_states (
    id            CHAR(26)    PRIMARY KEY,
    -- SHA-256 hex of the `state` that travelled in the authorization URL.
    state_hash    TEXT        NOT NULL,
    provider      TEXT        NOT NULL,
    -- SHA-256 hex of the PKCE `code_verifier`; the `code_challenge` derived from
    -- it is what went to the provider.
    code_verifier_hash TEXT   NOT NULL,
    -- Where to send the browser back to once the session is established, when the
    -- flow was started from a deep link. A relative path only — see the service,
    -- which refuses an absolute URL so this cannot become an open redirect.
    redirect_path TEXT,
    -- Non-NULL means the caller was already signed in: this is a *link* attempt.
    user_id       CHAR(26)    REFERENCES users (id) ON DELETE CASCADE,
    consumed_at   TIMESTAMPTZ,
    expires_at    TIMESTAMPTZ NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT oauth_states_id_ulid
        CHECK (id ~ '^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$'),
    CONSTRAINT oauth_states_provider
        CHECK (provider IN ('github', 'google')),
    CONSTRAINT oauth_states_state_hash_format
        CHECK (state_hash ~ '^[0-9a-f]{64}$'),
    CONSTRAINT oauth_states_verifier_hash_format
        CHECK (code_verifier_hash ~ '^[0-9a-f]{64}$'),
    -- A redirect target is a path we will hand to the browser; it must start with
    -- exactly one slash. "//evil.example" is protocol-relative, so it is refused
    -- here as well as in the service, belt and braces.
    CONSTRAINT oauth_states_redirect_path_relative
        CHECK (redirect_path IS NULL OR redirect_path ~ '^/[^/]')
);

-- Redemption is a single equality probe on the digest.
CREATE UNIQUE INDEX oauth_states_state_hash_key ON oauth_states (state_hash);

-- Expired and consumed states are garbage; this index is what a future cleanup
-- sweep would scan. Until then the rows are tiny and harmless.
CREATE INDEX oauth_states_expires_at_idx ON oauth_states (expires_at);

CREATE TABLE oauth_pending_sessions (
    id            CHAR(26)    PRIMARY KEY,
    -- SHA-256 hex of the limited-session token handed to the browser.
    token_hash    TEXT        NOT NULL,
    provider      TEXT        NOT NULL,
    subject       TEXT        NOT NULL,
    user_id       CHAR(26)    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    consumed_at   TIMESTAMPTZ,
    expires_at    TIMESTAMPTZ NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT oauth_pending_sessions_id_ulid
        CHECK (id ~ '^[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{26}$'),
    CONSTRAINT oauth_pending_sessions_provider
        CHECK (provider IN ('github', 'google')),
    CONSTRAINT oauth_pending_sessions_subject_length
        CHECK (char_length(subject) BETWEEN 1 AND 255),
    CONSTRAINT oauth_pending_sessions_token_hash_format
        CHECK (token_hash ~ '^[0-9a-f]{64}$')
);

CREATE UNIQUE INDEX oauth_pending_sessions_token_hash_key
    ON oauth_pending_sessions (token_hash);

-- The "return to the username step" probe: by the provider identity that still
-- names the account, whatever happened to the previous token.
CREATE UNIQUE INDEX oauth_pending_sessions_provider_subject_key
    ON oauth_pending_sessions (provider, subject);

-- Deleting a `users` row cascades here; PostgreSQL does not index FK columns
-- automatically, so the cascades get their own indexes.
CREATE INDEX oauth_states_user_id_idx ON oauth_states (user_id);
CREATE INDEX oauth_pending_sessions_user_id_idx ON oauth_pending_sessions (user_id);

COMMENT ON TABLE oauth_identities IS
    'A provider credential bound to one User; (provider, subject) is the only stable identity';
COMMENT ON COLUMN oauth_identities.provider IS
    'github | google; a closed set, never free text';
COMMENT ON COLUMN oauth_identities.subject IS
    'The provider''s immutable id for the account; the credential, never the email';
COMMENT ON COLUMN oauth_identities.email IS
    'The address the provider reported, informational only — never used to match an account';
COMMENT ON COLUMN oauth_identities.email_verified IS
    'Whether the provider asserted the address is verified; informational, not a linking rule';

COMMENT ON TABLE oauth_states IS
    'Single-use, short-lived OAuth `state` (CSRF) and PKCE verifier, stored as digests';
COMMENT ON COLUMN oauth_states.state_hash IS
    'SHA-256 hex of the state that travelled in the authorization URL';
COMMENT ON COLUMN oauth_states.code_verifier_hash IS
    'SHA-256 hex of the PKCE code_verifier; the challenge derived from it went to the provider';
COMMENT ON COLUMN oauth_states.redirect_path IS
    'Relative path to return to after sign-in; NULL means the default landing page';
COMMENT ON COLUMN oauth_states.user_id IS
    'Non-NULL when the round trip is an account-linking attempt by a signed-in user';

COMMENT ON TABLE oauth_pending_sessions IS
    'The limited session a first-time provider user holds until they choose a username';
COMMENT ON COLUMN oauth_pending_sessions.token_hash IS
    'SHA-256 hex of the limited-session token; it is opaque, so no protected endpoint accepts it';
COMMENT ON COLUMN oauth_pending_sessions.provider IS
    'The provider of the identity that created the account, for re-entry after an abandoned onboarding';
COMMENT ON COLUMN oauth_pending_sessions.consumed_at IS
    'When the limited session was exchanged for a full one; NULL means still usable';
