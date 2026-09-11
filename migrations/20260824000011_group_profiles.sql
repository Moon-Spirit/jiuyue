-- M12a: group profiles (description/avatar), per-member custom titles and the
-- group-local XP/level economy.
--
-- Extends the M11a group model without touching frozen wire shapes:
--   * `conversations.description` / `conversations.avatar` mirror the profile
--     surface: description is a plain ≤200-char blurb, avatar uses the SAME
--     data-URL scheme as user avatars (`data:image/{png|jpeg|webp};base64,…`,
--     ≤256 KB). Empty string means "cleared". Both are only meaningful for
--     kind='group'; direct/secret rows keep the defaults untouched.
--   * `conversation_members.custom_title` is an owner-set per-member display
--     title (≤16 chars, NULL/empty = none). It overrides the role/tier title.
--   * `group_xp` / `group_level` are the group-LOCAL economy (per member, per
--     conversation). The level is recomputed through the SAME domain pure fn
--     as the global XP ladder (`jiuyue_domain::progress`) inside every award
--     UPDATE, so the math is shared, never forked.
--   * `msg_xp_date` / `msg_xp_today` mirror the global xp_accounts daily
--     budget pattern: +10 group XP per plaintext text message, capped at
--     200/day per (conversation, user) with a date-rollover reset folded into
--     the award.
--
-- The award path is best-effort and post-commit in the WS layer; the REST
-- group surface stays authoritative.

ALTER TABLE conversations ADD COLUMN description text NOT NULL DEFAULT '';
ALTER TABLE conversations ADD COLUMN avatar text NOT NULL DEFAULT '';

ALTER TABLE conversation_members ADD COLUMN custom_title text NULL;
ALTER TABLE conversation_members ADD COLUMN group_xp bigint NOT NULL DEFAULT 0;
ALTER TABLE conversation_members ADD COLUMN group_level integer NOT NULL DEFAULT 1;
ALTER TABLE conversation_members ADD COLUMN msg_xp_date date NULL;
ALTER TABLE conversation_members ADD COLUMN msg_xp_today integer NOT NULL DEFAULT 0;
