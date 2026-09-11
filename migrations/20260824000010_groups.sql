-- M11a: group conversations -- named chats, owner/admin/member roles,
-- consent-based invites and owner moderation.
--
-- Extends the existing conversation model without touching direct/secret
-- semantics:
--   * `conversations.name` is nullable and only ever set for kind='group'
--     (direct/secret listings keep `name: null`, so their frozen shape is
--     unchanged).
--   * `conversation_members.role` gains the 'admin' tier between owner and
--     member. `conversation_members_role_check` is the Postgres auto-generated
--     name for the inline chat_core constraint (verified on the live DB);
--     drop + re-add is the documented way to widen it (same pattern as
--     `media_kind_check` in 20260824000009_audio.sql).
--   * `group_invites` is the consent ledger: an owner/admin invites a
--     username, the INVITEE accepts (becoming role='member') or declines.
--     The partial unique index keeps at most one PENDING invite per
--     (conversation, invitee); the (to_user) partial index backs the
--     `GET /api/groups/invites` inbox.
--
-- Invites are REST-authoritative. Live delivery rides fire-and-forget WS
-- frames (`group.invited` / `group.updated`) through the connection registry;
-- they are never persisted and never replayed by `sync.req`.

ALTER TABLE conversations ADD COLUMN name text NULL;

ALTER TABLE conversation_members DROP CONSTRAINT IF EXISTS conversation_members_role_check;
ALTER TABLE conversation_members ADD CONSTRAINT conversation_members_role_check
    CHECK (role IN ('owner', 'admin', 'member'));

CREATE TABLE group_invites (
    id uuid PRIMARY KEY,
    conversation_id bigint NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    from_user uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    to_user uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','accepted','declined')),
    created_at timestamptz NOT NULL DEFAULT now(),
    responded_at timestamptz NULL
);

CREATE UNIQUE INDEX group_invites_pending_uq ON group_invites(conversation_id, to_user) WHERE status = 'pending';
CREATE INDEX group_invites_to_user_pending_idx ON group_invites(to_user) WHERE status = 'pending';
