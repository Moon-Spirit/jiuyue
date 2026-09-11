-- M9a: voice/audio messages.
--
-- Extends the accepted `kind` vocabulary to include 'audio' for both the
-- authoritative `media` row and the `messages.kind` tag that mirrors it.
--
-- Notes:
--   * `media_kind_check` is the Postgres auto-generated name for the inline
--     column constraint from `20260824000008_media.sql`; drop + re-add is the
--     documented way to widen it.
--   * `messages.kind` carried NO check constraint before this migration
--     (chat_core only declared `kind text NOT NULL DEFAULT 'text'`). To honor
--     the M9a "extend CHECK to include 'audio'" contract we add an explicit
--     constraint covering every kind the send pipeline can write today:
--     text (plain), e2ee (secret chats), image/video (M8 media) and audio.
--
-- The audio byte-level pipeline is unchanged: bytes are stored verbatim and
-- `duration_ms` rides only on the client-supplied wire hint inside the
-- encrypted MediaRef envelope, never as a column here.

ALTER TABLE media DROP CONSTRAINT IF EXISTS media_kind_check;
ALTER TABLE media ADD CONSTRAINT media_kind_check
    CHECK (kind IN ('image', 'video', 'audio'));

ALTER TABLE messages DROP CONSTRAINT IF EXISTS messages_kind_check;
ALTER TABLE messages ADD CONSTRAINT messages_kind_check
    CHECK (kind IN ('text', 'e2ee', 'image', 'video', 'audio'));
