-- M8: media (image/video) attachments.
--
-- DEVIATION NOTE (deliberate): the M8 task sketch named this file
-- `20260824000007_media.sql`, but version 20260824000007 is already taken by
-- `20260824000007_profiles.sql` (M7). sqlx keys `_sqlx_migrations` by the
-- leading version number, so reusing 000007 would collide and fail the
-- migrator at startup. This migration therefore takes the next free version.
--
-- The `media` row is the authoritative metadata for one uploaded blob; the
-- bytes themselves live on local disk at
-- `{JIUYUE_MEDIA_DIR}/{yyyy}/{mm}/{id}.{ext}` (path is reconstructed from
-- `created_at` + `mime`, never stored). `JIUYUE_MEDIA_DIR` defaults to
-- `./data/media`.
--
--   * owner_id — FK to the uploader. msg.send only accepts a media id whose
--     owner matches the authenticated sender; ownership is checked in the
--     WS send pipeline, not here.
--   * kind      — 'image' | 'video', decided by MAGIC-BYTE sniffing at
--     upload (never trusted from Content-Type). It becomes the `messages.kind`
--     of any message that references this media.
--   * mime      — canonical content type (e.g. image/png, video/mp4),
--     echoed back on upload and as the Content-Type of GET /api/media/{id}.
--   * bytes     — exact stored file size; capped per kind at upload time.
--   * file_name — sanitized client-supplied display name (fallback "file").

CREATE TABLE media (
  id uuid PRIMARY KEY,
  owner_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  kind text NOT NULL CHECK (kind IN ('image','video')),
  mime text NOT NULL,
  bytes bigint NOT NULL,
  file_name text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX media_owner_idx ON media (owner_id);
